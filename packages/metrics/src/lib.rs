#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::significant_drop_tightening)]

//! Lightweight in-process metrics for Bcode runtime diagnostics.

pub mod dashboard;

use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const HISTOGRAM_BUCKETS_MS: &[u64] = &[
    1, 2, 5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000,
];
const DEFAULT_MAX_EVENTS: usize = 10_000;
const METRICS_EVENT_LOG_SCHEMA_VERSION: u32 = 1;
const DEFAULT_METRICS_SEGMENT_MAX_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_METRICS_TOTAL_MAX_BYTES: u64 = 128 * 1024 * 1024;
const DEFAULT_METRICS_RECENT_READ_MAX_BYTES: u64 = 16 * 1024 * 1024;
const METRICS_MANIFEST_FILE_NAME: &str = "manifest.json";
const METRICS_EVENT_QUEUE_CAPACITY: usize = 8_192;

/// Metric labels used for filtering and grouping dashboard views.
pub type MetricLabels = BTreeMap<String, String>;

/// Ambient context labels applied to metrics recorded in the current async task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsContext {
    labels: MetricLabels,
}

tokio::task_local! {
    static CURRENT_METRICS_CONTEXT: MetricsContext;
}

/// Return the metrics context active for the current async task.
#[must_use]
pub fn current_metrics_context() -> MetricsContext {
    CURRENT_METRICS_CONTEXT
        .try_with(Clone::clone)
        .unwrap_or_default()
}

/// Run a future with ambient metrics labels applied to all recording calls.
///
/// Existing task-local labels are preserved unless `context` explicitly overrides them.
pub async fn scope_metrics_context<F, T>(context: MetricsContext, future: F) -> T
where
    F: Future<Output = T>,
{
    let scoped_context = current_metrics_context().merge_context(context);
    CURRENT_METRICS_CONTEXT.scope(scoped_context, future).await
}

/// Shared metrics registry handle.
#[derive(Debug, Clone)]
pub struct MetricsRegistry {
    inner: MetricsRegistryInner,
}

#[derive(Debug, Clone)]
enum MetricsRegistryInner {
    Disabled,
    Enabled(Arc<Mutex<MetricsState>>),
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::in_memory()
    }
}

/// Persistent metrics event-log configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsEventLogConfig {
    /// Maximum size for one JSONL segment before rotating.
    pub segment_max_bytes: u64,
    /// Maximum total bytes to retain across closed JSONL segments and the active segment.
    pub total_max_bytes: u64,
    /// Maximum bytes to read while building a recent event report.
    pub recent_read_max_bytes: u64,
}

impl Default for MetricsEventLogConfig {
    fn default() -> Self {
        Self {
            segment_max_bytes: DEFAULT_METRICS_SEGMENT_MAX_BYTES,
            total_max_bytes: DEFAULT_METRICS_TOTAL_MAX_BYTES,
            recent_read_max_bytes: DEFAULT_METRICS_RECENT_READ_MAX_BYTES,
        }
    }
}

/// Health counters for queued persistent metric events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsPersistenceStatus {
    /// Number of events rejected because the bounded queue was full or disconnected.
    pub dropped_events: u64,
    /// Whether the background writer observed a persistence failure.
    pub writer_failed: bool,
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

/// Derived performance analysis for a metrics report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsAnalysis {
    /// Slowest histogram metrics by cumulative observed value.
    pub hotspots: Vec<MetricHotspot>,
    /// Slowest recent metric streams grouped by metric name and exact label set.
    #[serde(default)]
    pub label_hotspots: Vec<MetricLabelHotspot>,
    /// Metrics that look suspicious based on simple threshold heuristics.
    pub anomalies: Vec<MetricAnomaly>,
}

/// Recent-event hotspot for one metric plus label set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricLabelHotspot {
    /// Metric name.
    pub name: String,
    /// Labels attached to the grouped events.
    pub labels: MetricLabels,
    /// Number of recent samples.
    pub count: u64,
    /// Sum of recent sample values.
    pub total: u64,
    /// Maximum recent sample value.
    pub max: u64,
}

/// Aggregate hotspot for one histogram metric.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricHotspot {
    /// Metric name.
    pub name: String,
    /// Number of observations.
    pub count: u64,
    /// Sum of observed values.
    pub total: u64,
    /// Average observed value.
    pub average: u64,
    /// Maximum observed value.
    pub max: u64,
    /// Approximate p95 from fixed histogram buckets.
    #[serde(default)]
    pub p95: Option<u64>,
}

/// Potential metrics anomaly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricAnomaly {
    /// Severity, for example `info`, `warning`, or `critical`.
    pub severity: String,
    /// Stable anomaly code.
    pub code: String,
    /// Human-readable summary.
    pub message: String,
    /// Metric name associated with this anomaly.
    pub metric: String,
    /// Supporting labels or dimensions.
    #[serde(default)]
    pub labels: MetricLabels,
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

/// Low-friction RAII metrics span.
#[derive(Debug)]
pub struct MetricsSpan {
    metrics: MetricsRegistry,
    name: String,
    labels: MetricLabels,
    started_at: Instant,
    finished: bool,
}

#[derive(Debug, Default)]
struct MetricsState {
    counters: BTreeMap<String, u64>,
    gauges: BTreeMap<String, i64>,
    histograms: BTreeMap<String, Histogram>,
    descriptors: BTreeMap<String, MetricDescriptor>,
    events: Vec<MetricEvent>,
    event_log: Option<MetricsEventLog>,
    max_events: usize,
}

#[derive(Debug, Clone)]
struct MetricsEventLog {
    root_dir: PathBuf,
    writer: Arc<MetricsEventWriter>,
}

#[derive(Debug)]
enum MetricsWriterCommand {
    Event(MetricEvent),
    Flush(mpsc::Sender<()>),
    Shutdown(mpsc::Sender<()>),
}

#[derive(Debug)]
struct MetricsEventWriter {
    sender: mpsc::SyncSender<MetricsWriterCommand>,
    dropped_events: AtomicU64,
    failed: Arc<AtomicBool>,
}

static METRICS_EVENT_WRITERS: OnceLock<Mutex<BTreeMap<PathBuf, Arc<MetricsEventWriter>>>> =
    OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MetricsEventLogManifest {
    schema_version: u32,
    active_segment: String,
    segments: Vec<MetricsEventLogSegment>,
    event_count: u64,
    total_segment_bytes: u64,
    started_unix_ms: u64,
    updated_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MetricsEventLogSegment {
    name: String,
    started_unix_ms: u64,
    ended_unix_ms: Option<u64>,
    event_count: u64,
    bytes: u64,
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

impl MetricsContext {
    /// Create an empty metrics context.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return context labels.
    #[must_use]
    pub const fn labels(&self) -> &MetricLabels {
        &self.labels
    }

    /// Return a copy of this context with one added label.
    #[must_use]
    pub fn with_label(mut self, key: impl Into<String>, value: &impl ToString) -> Self {
        self.labels.insert(key.into(), value.to_string());
        self
    }

    /// Return a copy of this context with `session_id` set.
    #[must_use]
    pub fn with_session_id(self, session_id: &impl ToString) -> Self {
        self.with_label("session_id", session_id)
    }

    /// Return a copy of this context with `turn_id` set.
    #[must_use]
    pub fn with_turn_id(self, turn_id: &impl ToString) -> Self {
        self.with_label("turn_id", turn_id)
    }

    /// Return a copy of this context with `client_id` set.
    #[must_use]
    pub fn with_client_id(self, client_id: &impl ToString) -> Self {
        self.with_label("client_id", client_id)
    }

    /// Merge explicit labels with this context, preserving explicit values on conflicts.
    #[must_use]
    pub fn merged_labels(&self, explicit: MetricLabels) -> MetricLabels {
        let mut labels = self.labels.clone();
        labels.extend(explicit);
        labels
    }

    fn merge_context(mut self, explicit: Self) -> Self {
        self.labels.extend(explicit.labels);
        self
    }
}

impl MetricsRegistry {
    /// Load a metrics report from a persisted metrics event-log directory or legacy file hint.
    #[must_use]
    pub fn report_from_event_log_path(
        event_log_path: impl AsRef<Path>,
        config: MetricsEventLogConfig,
        max_events: usize,
    ) -> MetricsReport {
        let root = metrics_event_log_root(event_log_path.as_ref());
        let _ = MetricsEventLog::new(&root, config).flush();
        let log = SynchronousMetricsEventLog::new(root, config);
        let events = log.read_recent_events(max_events);
        MetricsReport {
            generated_at_unix_ms: current_unix_millis(),
            snapshot: snapshot_from_events(&events),
            descriptors: descriptors_from_events(&events),
            events,
        }
    }

    /// Create a disabled metrics registry. Recording calls become cheap no-ops.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            inner: MetricsRegistryInner::Disabled,
        }
    }

    /// Create an in-memory metrics registry without filesystem persistence.
    #[must_use]
    pub fn in_memory() -> Self {
        Self {
            inner: MetricsRegistryInner::Enabled(Arc::new(Mutex::new(MetricsState {
                max_events: DEFAULT_MAX_EVENTS,
                ..MetricsState::default()
            }))),
        }
    }

    /// Create a metrics registry that persists timeline events near `event_log_path`.
    ///
    /// Paths ending in a file name such as `events.jsonl` are treated as legacy-compatible hints;
    /// segmented metrics are stored in the parent directory.
    #[must_use]
    pub fn with_event_log(event_log_path: impl Into<PathBuf>) -> Self {
        Self::with_event_log_config(event_log_path, MetricsEventLogConfig::default())
    }

    /// Create a metrics registry that persists timeline events with explicit log settings.
    #[must_use]
    pub fn with_event_log_config(
        event_log_path: impl Into<PathBuf>,
        config: MetricsEventLogConfig,
    ) -> Self {
        let path = event_log_path.into();
        let root_dir = metrics_event_log_root(&path);
        Self {
            inner: MetricsRegistryInner::Enabled(Arc::new(Mutex::new(MetricsState {
                event_log: Some(MetricsEventLog::new(&root_dir, config)),
                max_events: DEFAULT_MAX_EVENTS,
                ..MetricsState::default()
            }))),
        }
    }

    /// Return a copy of this registry with a different recent-event report cap.
    #[must_use]
    pub fn with_max_events(self, max_events: usize) -> Self {
        if let MetricsRegistryInner::Enabled(inner) = &self.inner
            && let Ok(mut state) = inner.lock()
        {
            state.max_events = max_events;
        }
        self
    }

    /// Return whether this registry records metrics.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        matches!(self.inner, MetricsRegistryInner::Enabled(_))
    }

    /// Return the metrics context active for the current async task.
    #[must_use]
    pub fn current_context(&self) -> MetricsContext {
        current_metrics_context()
    }

    /// Run a future with ambient labels applied to all metrics recorded in this async task.
    pub async fn in_context<F, T>(&self, context: MetricsContext, future: F) -> T
    where
        F: Future<Output = T>,
    {
        let _ = self;
        scope_metrics_context(context, future).await
    }

    /// Start a named metrics span that records duration and count when finished or dropped.
    #[must_use]
    pub fn span(&self, name: impl Into<String>) -> MetricsSpan {
        MetricsSpan {
            metrics: self.clone(),
            name: name.into(),
            labels: MetricLabels::new(),
            started_at: Instant::now(),
            finished: false,
        }
    }

    /// Increment a counter by one.
    pub fn increment_counter(&self, key: impl Into<String>) {
        self.add_counter(key, 1);
    }

    /// Time an async operation and record its duration with the supplied labels.
    pub async fn time_async<F, T>(
        &self,
        name: impl Into<String>,
        labels: MetricLabels,
        future: F,
    ) -> T
    where
        F: Future<Output = T>,
    {
        let span = self.span(name).labels(labels);
        let output = future.await;
        span.finish();
        output
    }

    /// Time an async operation returning `Result` and record an outcome label.
    ///
    /// # Errors
    ///
    /// Returns the wrapped future's error unchanged.
    pub async fn time_result_async<F, T, E>(
        &self,
        name: impl Into<String>,
        labels: MetricLabels,
        future: F,
    ) -> Result<T, E>
    where
        F: Future<Output = Result<T, E>>,
    {
        let span = self.span(name).labels(labels);
        let output = future.await;
        span.finish_result(&output);
        output
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
        let MetricsRegistryInner::Enabled(inner) = &self.inner else {
            return;
        };
        let key = key.into();
        let labels = current_metrics_context().merged_labels(labels);
        let Ok(mut state) = inner.lock() else {
            return;
        };
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
        let MetricsRegistryInner::Enabled(inner) = &self.inner else {
            return;
        };
        let key = key.into();
        let labels = current_metrics_context().merged_labels(labels);
        let Ok(mut state) = inner.lock() else {
            return;
        };
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
        let MetricsRegistryInner::Enabled(inner) = &self.inner else {
            return;
        };
        let key = key.into();
        let labels = current_metrics_context().merged_labels(labels);
        let Ok(mut state) = inner.lock() else {
            return;
        };
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
        let MetricsRegistryInner::Enabled(inner) = &self.inner else {
            return;
        };
        let key = key.into();
        let labels = current_metrics_context().merged_labels(labels);
        let Ok(mut state) = inner.lock() else {
            return;
        };
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
        let MetricsRegistryInner::Enabled(inner) = &self.inner else {
            return MetricsSnapshot::default();
        };
        let Ok(state) = inner.lock() else {
            return MetricsSnapshot::default();
        };
        state.snapshot()
    }

    /// Flush queued persistent events and return writer health.
    ///
    /// In-memory and disabled registries return immediately.
    #[must_use]
    pub fn flush_persistence(&self) -> MetricsPersistenceStatus {
        let MetricsRegistryInner::Enabled(inner) = &self.inner else {
            return MetricsPersistenceStatus::default();
        };
        let Ok(state) = inner.lock() else {
            return MetricsPersistenceStatus {
                writer_failed: true,
                ..MetricsPersistenceStatus::default()
            };
        };
        state
            .event_log
            .as_ref()
            .map_or_else(MetricsPersistenceStatus::default, MetricsEventLog::flush)
    }

    /// Flush accepted events, stop the process-local writer for this root, and return its health.
    ///
    /// Callers must quiesce all registries targeting the same root before shutdown. Recording through
    /// an older registry after this method returns is rejected and counted as dropped telemetry.
    #[must_use]
    pub fn shutdown_persistence(&self) -> MetricsPersistenceStatus {
        let MetricsRegistryInner::Enabled(inner) = &self.inner else {
            return MetricsPersistenceStatus::default();
        };
        let Ok(state) = inner.lock() else {
            return MetricsPersistenceStatus {
                writer_failed: true,
                ..MetricsPersistenceStatus::default()
            };
        };
        state
            .event_log
            .as_ref()
            .map_or_else(MetricsPersistenceStatus::default, MetricsEventLog::shutdown)
    }

    /// Return a rich metrics report with aggregate snapshots and recent events.
    #[must_use]
    pub fn report(&self) -> MetricsReport {
        let MetricsRegistryInner::Enabled(inner) = &self.inner else {
            return MetricsReport {
                generated_at_unix_ms: current_unix_millis(),
                ..MetricsReport::default()
            };
        };
        let Ok(state) = inner.lock() else {
            return MetricsReport {
                generated_at_unix_ms: current_unix_millis(),
                ..MetricsReport::default()
            };
        };
        MetricsReport {
            generated_at_unix_ms: current_unix_millis(),
            snapshot: state.snapshot(),
            events: state.report_events(),
            descriptors: state.descriptors.clone(),
        }
    }
    /// Analyze the current metrics report for hotspots and simple anomalies.
    #[must_use]
    pub fn analysis(&self) -> MetricsAnalysis {
        analyze_metrics_report(&self.report())
    }
}

/// Analyze a metrics report for hotspots and simple anomalies.
#[must_use]
pub fn analyze_metrics_report(report: &MetricsReport) -> MetricsAnalysis {
    let mut hotspots: Vec<MetricHotspot> = report
        .snapshot
        .histograms
        .iter()
        .map(|(name, histogram)| histogram_hotspot(name, histogram))
        .collect();
    hotspots.sort_by_key(|hotspot| Reverse(hotspot.total));
    hotspots.truncate(25);

    let label_hotspots = label_hotspots(report);

    let mut anomalies = Vec::new();
    for hotspot in &hotspots {
        if hotspot.max >= 30_000 || hotspot.p95.is_some_and(|p95| p95 >= 10_000) {
            anomalies.push(MetricAnomaly {
                severity: "warning".to_owned(),
                code: "slow_metric".to_owned(),
                message: format!(
                    "{} is slow: avg={} max={} p95={}",
                    hotspot.name,
                    hotspot.average,
                    hotspot.max,
                    hotspot
                        .p95
                        .map_or_else(|| "<unknown>".to_owned(), |value| value.to_string())
                ),
                metric: hotspot.name.clone(),
                labels: MetricLabels::new(),
            });
        }
    }
    for (name, value) in &report.snapshot.counters {
        if name.ends_with("errors_total") && *value > 0 {
            anomalies.push(MetricAnomaly {
                severity: "warning".to_owned(),
                code: "metric_errors".to_owned(),
                message: format!("{name} has recorded {value} errors"),
                metric: name.clone(),
                labels: MetricLabels::new(),
            });
        }
    }
    for label_hotspot in &label_hotspots {
        if label_hotspot.max >= 10_000 {
            anomalies.push(MetricAnomaly {
                severity: "warning".to_owned(),
                code: "slow_label_group".to_owned(),
                message: format!(
                    "{} label group is slow: count={} total={} max={}",
                    label_hotspot.name, label_hotspot.count, label_hotspot.total, label_hotspot.max
                ),
                metric: label_hotspot.name.clone(),
                labels: label_hotspot.labels.clone(),
            });
        }
    }
    MetricsAnalysis {
        hotspots,
        label_hotspots,
        anomalies,
    }
}

fn label_hotspots(report: &MetricsReport) -> Vec<MetricLabelHotspot> {
    let mut grouped: BTreeMap<(String, MetricLabels), MetricLabelHotspot> = BTreeMap::new();
    for event in &report.events {
        if !matches!(event.kind, MetricKind::Histogram) || event.value < 0 {
            continue;
        }
        let value = u64::try_from(event.value).unwrap_or(u64::MAX);
        let entry = grouped
            .entry((event.name.clone(), event.labels.clone()))
            .or_insert_with(|| MetricLabelHotspot {
                name: event.name.clone(),
                labels: event.labels.clone(),
                count: 0,
                total: 0,
                max: 0,
            });
        entry.count = entry.count.saturating_add(1);
        entry.total = entry.total.saturating_add(value);
        entry.max = entry.max.max(value);
    }
    let mut hotspots = grouped.into_values().collect::<Vec<_>>();
    hotspots.sort_by_key(|hotspot| Reverse(hotspot.total));
    hotspots.truncate(25);
    hotspots
}

fn histogram_hotspot(name: &str, histogram: &HistogramSnapshot) -> MetricHotspot {
    let average = histogram.sum.checked_div(histogram.count).unwrap_or(0);
    MetricHotspot {
        name: name.to_owned(),
        count: histogram.count,
        total: histogram.sum,
        average,
        max: histogram.max.unwrap_or(0),
        p95: histogram_percentile(histogram, 95),
    }
}

fn histogram_percentile(histogram: &HistogramSnapshot, percentile: u64) -> Option<u64> {
    if histogram.count == 0 {
        return None;
    }
    let target = histogram
        .count
        .saturating_mul(percentile)
        .saturating_add(99)
        / 100;
    histogram
        .buckets
        .iter()
        .find(|bucket| bucket.count >= target)
        .map(|bucket| bucket.le)
        .or(histogram.max)
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

impl MetricsSpan {
    /// Add a string-like label to this span.
    #[must_use]
    pub fn label(mut self, key: impl Into<String>, value: &impl ToString) -> Self {
        self.labels.insert(key.into(), value.to_string());
        self
    }

    /// Add many labels to this span.
    #[must_use]
    pub fn labels(mut self, labels: MetricLabels) -> Self {
        self.labels.extend(labels);
        self
    }

    /// Finish this span with the default `ok` outcome.
    pub fn finish(mut self) {
        self.finish_with_outcome(&"ok");
    }

    /// Finish this span with an `ok` outcome.
    pub fn finish_ok(mut self) {
        self.finish_with_outcome(&"ok");
    }

    /// Finish this span with an `error` outcome.
    pub fn finish_err(mut self) {
        self.finish_with_outcome(&"error");
    }

    /// Finish this span based on a `Result` outcome.
    pub fn finish_result<T, E>(mut self, result: &Result<T, E>) {
        if result.is_ok() {
            self.finish_with_outcome(&"ok");
        } else {
            self.finish_with_outcome(&"error");
        }
    }

    /// Finish this span with an outcome label.
    pub fn finish_with_outcome(&mut self, outcome: &impl ToString) {
        if self.finished {
            return;
        }
        self.finished = true;
        let mut labels = self.labels.clone();
        labels.insert("outcome".to_owned(), outcome.to_string());
        self.metrics
            .increment_counter_with_name_and_labels(format!("{}.total", self.name), labels.clone());
        self.metrics.record_histogram_with_labels(
            format!("{}.duration_ms", self.name),
            u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            labels,
        );
    }
}

impl Drop for MetricsSpan {
    fn drop(&mut self) {
        self.finish_with_outcome(&"ok");
    }
}

impl MetricsRegistry {
    fn increment_counter_with_name_and_labels(&self, key: String, labels: MetricLabels) {
        self.add_counter_with_labels(key, 1, labels);
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
        if let Some(log) = &self.event_log {
            log.append_event(&event);
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
        self.events.clone()
    }
}

impl MetricsEventLog {
    fn new(root_dir: &Path, config: MetricsEventLogConfig) -> Self {
        let writers = METRICS_EVENT_WRITERS.get_or_init(|| Mutex::new(BTreeMap::new()));
        let normalized_root = normalize_metrics_root(root_dir);
        let mut writers = writers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(writer) = writers.get(&normalized_root) {
            return Self {
                root_dir: normalized_root,
                writer: Arc::clone(writer),
            };
        }
        let (sender, receiver) = mpsc::sync_channel(METRICS_EVENT_QUEUE_CAPACITY);
        let failed = Arc::new(AtomicBool::new(false));
        let writer_failed = Arc::clone(&failed);
        let writer = Arc::new(MetricsEventWriter {
            sender,
            dropped_events: AtomicU64::new(0),
            failed,
        });
        writers.insert(normalized_root.clone(), Arc::clone(&writer));
        let writer_root = normalized_root.clone();
        thread::Builder::new()
            .name("bcode-metrics-writer".to_string())
            .spawn(move || {
                let log = SynchronousMetricsEventLog::new(writer_root, config);
                'writer: while let Ok(command) = receiver.recv() {
                    match command {
                        MetricsWriterCommand::Event(first) => {
                            let mut events = vec![first];
                            let mut pending_barrier = None;
                            let mut shutdown = false;
                            while events.len() < 256 {
                                match receiver.try_recv() {
                                    Ok(MetricsWriterCommand::Event(event)) => events.push(event),
                                    Ok(MetricsWriterCommand::Flush(acknowledge)) => {
                                        pending_barrier = Some(acknowledge);
                                        break;
                                    }
                                    Ok(MetricsWriterCommand::Shutdown(acknowledge)) => {
                                        pending_barrier = Some(acknowledge);
                                        shutdown = true;
                                        break;
                                    }
                                    Err(
                                        mpsc::TryRecvError::Empty
                                        | mpsc::TryRecvError::Disconnected,
                                    ) => break,
                                }
                            }
                            if log.try_append_events(&events).is_err() {
                                writer_failed.store(true, Ordering::Release);
                            }
                            if let Some(acknowledge) = pending_barrier {
                                let _ = acknowledge.send(());
                            }
                            if shutdown {
                                break 'writer;
                            }
                        }
                        MetricsWriterCommand::Flush(acknowledge) => {
                            let _ = acknowledge.send(());
                        }
                        MetricsWriterCommand::Shutdown(acknowledge) => {
                            let _ = acknowledge.send(());
                            break 'writer;
                        }
                    }
                }
            })
            .expect("metrics writer thread should start");
        Self {
            root_dir: normalized_root,
            writer,
        }
    }

    fn append_event(&self, event: &MetricEvent) {
        // Preserve already accepted telemetry under pressure: reject the newest event rather than
        // blocking the caller or displacing an event whose persistence was previously promised.
        if self
            .writer
            .sender
            .try_send(MetricsWriterCommand::Event(event.clone()))
            .is_err()
        {
            self.writer.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn flush(&self) -> MetricsPersistenceStatus {
        let (sender, receiver) = mpsc::channel();
        if self
            .writer
            .sender
            .send(MetricsWriterCommand::Flush(sender))
            .is_ok()
        {
            let _ = receiver.recv();
        }
        MetricsPersistenceStatus {
            dropped_events: self.writer.dropped_events.load(Ordering::Acquire),
            writer_failed: self.writer.failed.load(Ordering::Acquire),
        }
    }

    fn shutdown(&self) -> MetricsPersistenceStatus {
        let writers = METRICS_EVENT_WRITERS.get_or_init(|| Mutex::new(BTreeMap::new()));
        let mut writers = writers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if writers
            .get(&self.root_dir)
            .is_some_and(|writer| Arc::ptr_eq(writer, &self.writer))
        {
            writers.remove(&self.root_dir);
        }
        drop(writers);
        let (sender, receiver) = mpsc::channel();
        if self
            .writer
            .sender
            .send(MetricsWriterCommand::Shutdown(sender))
            .is_ok()
        {
            let _ = receiver.recv();
        }
        MetricsPersistenceStatus {
            dropped_events: self.writer.dropped_events.load(Ordering::Acquire),
            writer_failed: self.writer.failed.load(Ordering::Acquire),
        }
    }
}

#[derive(Debug, Clone)]
struct SynchronousMetricsEventLog {
    root_dir: PathBuf,
    manifest_path: PathBuf,
    config: MetricsEventLogConfig,
}

impl SynchronousMetricsEventLog {
    fn new(root_dir: PathBuf, config: MetricsEventLogConfig) -> Self {
        Self {
            manifest_path: root_dir.join(METRICS_MANIFEST_FILE_NAME),
            root_dir,
            config,
        }
    }

    fn try_append_events(&self, events: &[MetricEvent]) -> std::io::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        fs::create_dir_all(&self.root_dir)?;
        let now_ms = current_unix_millis();
        let mut manifest = self.load_or_initialize_manifest(now_ms);
        let lines = events
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .map_err(std::io::Error::other)?;
        let line_bytes = lines.iter().fold(0_u64, |total, line| {
            total
                .saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX))
                .saturating_add(1)
        });
        let active_path = self.root_dir.join(&manifest.active_segment);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&active_path)?;
        for line in lines {
            writeln!(file, "{line}")?;
        }
        let active_bytes = fs::metadata(&active_path).map_or(line_bytes, |metadata| metadata.len());
        let event_count = u64::try_from(events.len()).unwrap_or(u64::MAX);
        if let Some(segment) = manifest
            .segments
            .iter_mut()
            .find(|segment| segment.name == manifest.active_segment)
        {
            segment.event_count = segment.event_count.saturating_add(event_count);
            segment.bytes = active_bytes;
        }
        manifest.event_count = manifest.event_count.saturating_add(event_count);
        manifest.updated_unix_ms = now_ms;
        manifest.total_segment_bytes = compute_total_segment_bytes(&self.root_dir, &manifest);
        if active_bytes >= self.config.segment_max_bytes.max(1) {
            self.rotate_manifest(&mut manifest, now_ms);
        }
        self.prune_closed_segments(&mut manifest)?;
        manifest.total_segment_bytes = compute_total_segment_bytes(&self.root_dir, &manifest);
        write_json_pretty(&self.manifest_path, &manifest)
    }

    fn read_recent_events(&self, max_events: usize) -> Vec<MetricEvent> {
        let Some(manifest) = self.load_manifest() else {
            return Vec::new();
        };
        let mut segment_batches = Vec::new();
        let mut bytes_read = 0_u64;
        for segment in manifest.segments.iter().rev() {
            if bytes_read >= self.config.recent_read_max_bytes {
                break;
            }
            let path = self.root_dir.join(&segment.name);
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            let remaining_bytes = self.config.recent_read_max_bytes.saturating_sub(bytes_read);
            let read_limit = metadata.len().min(remaining_bytes);
            bytes_read = bytes_read.saturating_add(read_limit);
            segment_batches.push(read_jsonl_events_tail(&path, read_limit));
            if segment_batches.iter().map(Vec::len).sum::<usize>() >= max_events {
                break;
            }
        }
        let mut events = Vec::new();
        for mut segment_events in segment_batches.into_iter().rev() {
            events.append(&mut segment_events);
        }
        if events.len() > max_events {
            let excess = events.len() - max_events;
            events.drain(0..excess);
        }
        events
    }

    fn load_or_initialize_manifest(&self, now_ms: u64) -> MetricsEventLogManifest {
        self.load_manifest()
            .filter(|manifest| manifest.schema_version == METRICS_EVENT_LOG_SCHEMA_VERSION)
            .unwrap_or_else(|| initial_manifest(now_ms))
    }

    fn load_manifest(&self) -> Option<MetricsEventLogManifest> {
        fs::read(&self.manifest_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    }

    fn rotate_manifest(&self, manifest: &mut MetricsEventLogManifest, now_ms: u64) {
        if let Some(active) = manifest
            .segments
            .iter_mut()
            .find(|segment| segment.name == manifest.active_segment)
        {
            active.ended_unix_ms = Some(now_ms);
        }
        let next_index = next_segment_index(&manifest.segments);
        let segment_name = format!("events_{next_index}.jsonl");
        manifest.active_segment.clone_from(&segment_name);
        manifest.segments.push(MetricsEventLogSegment {
            name: segment_name,
            started_unix_ms: now_ms,
            ended_unix_ms: None,
            event_count: 0,
            bytes: 0,
        });
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root_dir.join(&manifest.active_segment));
    }

    fn prune_closed_segments(&self, manifest: &mut MetricsEventLogManifest) -> std::io::Result<()> {
        let max_bytes = self
            .config
            .total_max_bytes
            .max(self.config.segment_max_bytes.max(1));
        while manifest.total_segment_bytes > max_bytes {
            let Some(index) = manifest
                .segments
                .iter()
                .position(|segment| segment.ended_unix_ms.is_some())
            else {
                break;
            };
            let segment = manifest.segments.remove(index);
            let path = self.root_dir.join(&segment.name);
            if let Err(error) = fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(error);
            }
            manifest.total_segment_bytes = compute_total_segment_bytes(&self.root_dir, manifest);
        }
        Ok(())
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

/// Build an aggregate snapshot from metric events.
#[must_use]
pub fn snapshot_from_events(events: &[MetricEvent]) -> MetricsSnapshot {
    let mut counters = BTreeMap::new();
    let mut gauges = BTreeMap::new();
    let mut histograms = BTreeMap::<String, Histogram>::new();
    for event in events {
        match event.kind {
            MetricKind::Counter => {
                let value = u64::try_from(event.value).unwrap_or_default();
                let counter = counters.entry(event.name.clone()).or_insert(0_u64);
                *counter = counter.saturating_add(value);
            }
            MetricKind::Gauge => {
                gauges.insert(event.name.clone(), event.value);
            }
            MetricKind::Histogram => {
                if let Ok(value) = u64::try_from(event.value) {
                    histograms
                        .entry(event.name.clone())
                        .or_default()
                        .record(value);
                }
            }
            MetricKind::Event => {}
        }
    }
    MetricsSnapshot {
        counters,
        gauges,
        histograms: histograms
            .into_iter()
            .map(|(name, histogram)| (name, histogram.snapshot()))
            .collect(),
    }
}

/// Build metric descriptors from observed metric events.
#[must_use]
pub fn descriptors_from_events(events: &[MetricEvent]) -> BTreeMap<String, MetricDescriptor> {
    let mut descriptors = BTreeMap::new();
    for event in events {
        let descriptor =
            descriptors
                .entry(event.name.clone())
                .or_insert_with(|| MetricDescriptor {
                    name: event.name.clone(),
                    kind: event.kind,
                    unit: infer_unit(&event.name),
                    description: None,
                    label_keys: Vec::new(),
                });
        for key in event.labels.keys() {
            if !descriptor.label_keys.contains(key) {
                descriptor.label_keys.push(key.clone());
            }
        }
    }
    descriptors
}

fn normalize_metrics_root(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    }
}

fn metrics_event_log_root(path: &Path) -> PathBuf {
    if path.extension().is_some() {
        path.parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    } else {
        path.to_path_buf()
    }
}

fn initial_manifest(now_ms: u64) -> MetricsEventLogManifest {
    let active_segment = "events_0.jsonl".to_owned();
    MetricsEventLogManifest {
        schema_version: METRICS_EVENT_LOG_SCHEMA_VERSION,
        active_segment: active_segment.clone(),
        segments: vec![MetricsEventLogSegment {
            name: active_segment,
            started_unix_ms: now_ms,
            ended_unix_ms: None,
            event_count: 0,
            bytes: 0,
        }],
        event_count: 0,
        total_segment_bytes: 0,
        started_unix_ms: now_ms,
        updated_unix_ms: now_ms,
    }
}

fn next_segment_index(segments: &[MetricsEventLogSegment]) -> u64 {
    segments
        .iter()
        .filter_map(|segment| {
            segment
                .name
                .strip_prefix("events_")
                .and_then(|name| name.strip_suffix(".jsonl"))
                .and_then(|index| index.parse::<u64>().ok())
        })
        .max()
        .map_or(0, |index| index.saturating_add(1))
}

fn compute_total_segment_bytes(root_dir: &Path, manifest: &MetricsEventLogManifest) -> u64 {
    manifest
        .segments
        .iter()
        .map(|segment| {
            fs::metadata(root_dir.join(&segment.name)).map_or(segment.bytes, |m| m.len())
        })
        .sum()
}

fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, serde_json::to_vec_pretty(value)?)?;
    fs::rename(temp_path, path)
}

fn read_jsonl_events_tail(path: &Path, max_bytes: u64) -> Vec<MetricEvent> {
    let Ok(mut file) = fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(file_len) = file.seek(SeekFrom::End(0)) else {
        return Vec::new();
    };
    let start = file_len.saturating_sub(max_bytes);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let mut lines = BufReader::new(bytes.as_slice()).lines();
    if start > 0 {
        let _ = lines.next();
    }
    lines
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<MetricEvent>(&line).ok())
        .collect()
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
    use std::process::Command;

    const ABRUPT_CHILD_ENV: &str = "BCODE_METRICS_ABRUPT_CHILD_ROOT";

    #[test]
    fn disabled_registry_is_empty_and_noops() {
        let metrics = MetricsRegistry::disabled();
        metrics.increment_counter("counter");
        metrics.record_histogram("latency_ms", 5);

        assert!(!metrics.is_enabled());
        assert_eq!(metrics.snapshot(), MetricsSnapshot::default());
        assert!(metrics.report().events.is_empty());
    }

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

    #[test]
    fn span_records_duration_and_count() {
        let metrics = MetricsRegistry::default();
        {
            let _span = metrics.span("test.span").label("kind", &"unit");
        }

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.counters.get("test.span.total"), Some(&1));
        assert!(snapshot.histograms.contains_key("test.span.duration_ms"));
    }

    #[test]
    fn segmented_event_log_rotates_and_reports_chronologically() {
        let dir = tempfile::tempdir().expect("temp dir");
        let metrics = MetricsRegistry::with_event_log_config(
            dir.path().join("events.jsonl"),
            MetricsEventLogConfig {
                segment_max_bytes: 256,
                total_max_bytes: 16 * 1024,
                recent_read_max_bytes: 4096,
            },
        );

        for index in 0..20 {
            let mut labels = MetricLabels::new();
            labels.insert("index".to_owned(), index.to_string());
            metrics.record_event("test.event", index, labels);
        }

        let status = metrics.flush_persistence();
        assert_eq!(status, MetricsPersistenceStatus::default());
        let manifest_path = dir.path().join(METRICS_MANIFEST_FILE_NAME);
        let manifest: MetricsEventLogManifest =
            serde_json::from_slice(&fs::read(manifest_path).expect("manifest should exist"))
                .expect("manifest should parse");
        assert!(manifest.segments.len() > 1);
        assert!(manifest.active_segment.starts_with("events_"));

        let report = metrics.report();
        assert_eq!(report.events.len(), 20);
        let values: Vec<i64> = report.events.iter().map(|event| event.value).collect();
        assert_eq!(values, (0..20).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn ambient_context_labels_are_applied_to_metric_events() {
        let metrics = MetricsRegistry::default();
        let context = MetricsContext::new()
            .with_session_id(&"session-a")
            .with_turn_id(&"turn-1");

        metrics
            .in_context(context, async {
                let mut labels = MetricLabels::new();
                labels.insert("turn_id".to_owned(), "explicit-turn".to_owned());
                metrics.record_histogram_with_labels("model.poll.duration_ms", 42, labels);
                metrics.increment_counter("model.poll.iterations");
            })
            .await;

        let report = metrics.report();
        let histogram = report
            .events
            .iter()
            .find(|event| event.name == "model.poll.duration_ms")
            .expect("histogram event should be recorded");
        assert_eq!(
            histogram.labels.get("session_id"),
            Some(&"session-a".to_owned())
        );
        assert_eq!(
            histogram.labels.get("turn_id"),
            Some(&"explicit-turn".to_owned())
        );
        let counter = report
            .events
            .iter()
            .find(|event| event.name == "model.poll.iterations")
            .expect("counter event should be recorded");
        assert_eq!(
            counter.labels.get("session_id"),
            Some(&"session-a".to_owned())
        );
        assert_eq!(counter.labels.get("turn_id"), Some(&"turn-1".to_owned()));
    }

    #[tokio::test]
    async fn metrics_span_inherits_ambient_context_labels() {
        let metrics = MetricsRegistry::default();
        let context = MetricsContext::new().with_session_id(&"session-a");

        metrics
            .in_context(context, async {
                metrics.span("session.operation").finish_ok();
            })
            .await;

        let report = metrics.report();
        assert!(
            report
                .events
                .iter()
                .any(|event| event.name == "session.operation.total"
                    && event.labels.get("session_id") == Some(&"session-a".to_owned())
                    && event.labels.get("outcome") == Some(&"ok".to_owned()))
        );
        assert!(
            report
                .events
                .iter()
                .any(|event| event.name == "session.operation.duration_ms"
                    && event.labels.get("session_id") == Some(&"session-a".to_owned())
                    && event.labels.get("outcome") == Some(&"ok".to_owned()))
        );
    }

    #[test]
    fn concurrent_registries_share_one_writer_without_malformed_lines() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("events.jsonl");
        let mut workers = Vec::new();
        for worker in 0..8 {
            let path = path.clone();
            workers.push(thread::spawn(move || {
                let metrics = MetricsRegistry::with_event_log(path);
                for index in 0..100 {
                    let mut labels = MetricLabels::new();
                    labels.insert("worker".to_owned(), worker.to_string());
                    metrics.record_event("concurrent.event", index, labels);
                }
                metrics.flush_persistence()
            }));
        }
        for worker in workers {
            assert_eq!(
                worker.join().expect("worker should join"),
                MetricsPersistenceStatus::default()
            );
        }

        let report = MetricsRegistry::report_from_event_log_path(
            &path,
            MetricsEventLogConfig::default(),
            1_000,
        );
        assert_eq!(report.events.len(), 800);
        let manifest: MetricsEventLogManifest = serde_json::from_slice(
            &fs::read(dir.path().join(METRICS_MANIFEST_FILE_NAME)).expect("manifest should exist"),
        )
        .expect("manifest should parse");
        assert_eq!(manifest.event_count, 800);
        for segment in manifest.segments {
            let file = fs::File::open(dir.path().join(segment.name)).expect("segment should open");
            for line in BufReader::new(file).lines() {
                let line = line.expect("line should read");
                serde_json::from_str::<MetricEvent>(&line)
                    .expect("line should be valid event JSON");
            }
        }
    }

    #[test]
    #[ignore = "manual performance benchmark"]
    fn benchmark_recording_modes() {
        const SAMPLES: u32 = 5_000;
        fn record_samples(metrics: &MetricsRegistry) -> u128 {
            let started = Instant::now();
            for value in 0..SAMPLES {
                metrics.record_event("benchmark.event", i64::from(value), MetricLabels::new());
            }
            started.elapsed().as_nanos()
        }
        fn per_sample(total_ns: u128) -> u128 {
            total_ns / u128::from(SAMPLES)
        }

        let disabled_ns = record_samples(&MetricsRegistry::disabled());
        let aggregate_ns = record_samples(&MetricsRegistry::in_memory());
        let dir = tempfile::tempdir().expect("temp dir");
        let queued = MetricsRegistry::with_event_log(dir.path().join("events.jsonl"));
        let queued_ns = record_samples(&queued);
        let persistence_started = Instant::now();
        let status = queued.shutdown_persistence();
        let persisted_ns = queued_ns.saturating_add(persistence_started.elapsed().as_nanos());
        assert_eq!(status, MetricsPersistenceStatus::default());
        let report = MetricsRegistry::report_from_event_log_path(
            dir.path().join("events.jsonl"),
            MetricsEventLogConfig::default(),
            usize::try_from(SAMPLES).expect("sample count"),
        );
        assert_eq!(
            report.events.len(),
            usize::try_from(SAMPLES).expect("sample count")
        );
        eprintln!(
            "metrics benchmark ({SAMPLES} samples): disabled={} ns/sample, aggregate={} ns/sample, queued={} ns/sample, persisted={} ns/sample",
            per_sample(disabled_ns),
            per_sample(aggregate_ns),
            per_sample(queued_ns),
            per_sample(persisted_ns),
        );
    }

    #[test]
    fn abrupt_process_exit_keeps_log_parseable_and_recoverable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let executable = std::env::current_exe().expect("current test executable");
        let status = Command::new(executable)
            .arg("--exact")
            .arg("tests::abrupt_process_exit_child")
            .arg("--nocapture")
            .env(ABRUPT_CHILD_ENV, dir.path())
            .status()
            .expect("abrupt child should run");
        assert!(status.success());

        let manifest_path = dir.path().join(METRICS_MANIFEST_FILE_NAME);
        let manifest: MetricsEventLogManifest = serde_json::from_slice(
            &fs::read(&manifest_path).expect("manifest should survive abrupt exit"),
        )
        .expect("manifest should remain valid JSON");
        assert!(manifest.event_count >= 20);
        for segment in &manifest.segments {
            let file = fs::File::open(dir.path().join(&segment.name)).expect("segment should open");
            for line in BufReader::new(file).lines() {
                let line = line.expect("line should read");
                serde_json::from_str::<MetricEvent>(&line)
                    .expect("line should remain complete JSON");
            }
        }

        let path = dir.path().join("events.jsonl");
        let recovered = MetricsRegistry::with_event_log(&path);
        recovered.record_event("recovered.event", 1, MetricLabels::new());
        assert_eq!(
            recovered.shutdown_persistence(),
            MetricsPersistenceStatus::default()
        );
        let report = MetricsRegistry::report_from_event_log_path(
            path,
            MetricsEventLogConfig::default(),
            1_000,
        );
        assert!(
            report
                .events
                .iter()
                .any(|event| event.name == "recovered.event")
        );
    }

    #[test]
    fn abrupt_process_exit_child() {
        let Ok(root) = std::env::var(ABRUPT_CHILD_ENV) else {
            return;
        };
        let metrics = MetricsRegistry::with_event_log(PathBuf::from(root).join("events.jsonl"));
        for index in 0..20 {
            metrics.record_event("durable.before.exit", index, MetricLabels::new());
        }
        assert_eq!(
            metrics.flush_persistence(),
            MetricsPersistenceStatus::default()
        );
        for index in 0..1_000 {
            metrics.record_event("queued.at.exit", index, MetricLabels::new());
        }
        std::process::exit(0);
    }

    #[test]
    fn full_queue_drops_newest_event_without_blocking() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .try_send(MetricsWriterCommand::Event(MetricEvent {
                unix_ms: 1,
                name: "accepted.event".to_owned(),
                kind: MetricKind::Event,
                value: 1,
                labels: MetricLabels::new(),
            }))
            .expect("first event should fill queue");
        let event_log = MetricsEventLog {
            root_dir: PathBuf::from("unused"),
            writer: Arc::new(MetricsEventWriter {
                sender,
                dropped_events: AtomicU64::new(0),
                failed: Arc::new(AtomicBool::new(false)),
            }),
        };

        event_log.append_event(&MetricEvent {
            unix_ms: 2,
            name: "rejected.event".to_owned(),
            kind: MetricKind::Event,
            value: 2,
            labels: MetricLabels::new(),
        });

        assert_eq!(event_log.writer.dropped_events.load(Ordering::Acquire), 1);
        let MetricsWriterCommand::Event(accepted) = receiver.try_recv().expect("accepted event")
        else {
            panic!("queue should retain first event");
        };
        assert_eq!(accepted.name, "accepted.event");
    }

    #[test]
    fn shutdown_flushes_accepted_events_and_rejects_stale_registries() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("events.jsonl");
        let metrics = MetricsRegistry::with_event_log(&path);
        for index in 0..20 {
            metrics.record_event("shutdown.event", index, MetricLabels::new());
        }

        assert_eq!(
            metrics.shutdown_persistence(),
            MetricsPersistenceStatus::default()
        );
        metrics.record_event("shutdown.rejected", 1, MetricLabels::new());
        let stale_status = metrics.flush_persistence();
        assert_eq!(stale_status.dropped_events, 1);
        assert!(!stale_status.writer_failed);

        let report = MetricsRegistry::report_from_event_log_path(
            &path,
            MetricsEventLogConfig::default(),
            100,
        );
        assert_eq!(report.events.len(), 20);
        assert!(
            report
                .events
                .iter()
                .all(|event| event.name == "shutdown.event")
        );

        let reopened = MetricsRegistry::with_event_log(&path);
        reopened.record_event("shutdown.reopened", 1, MetricLabels::new());
        assert_eq!(
            reopened.shutdown_persistence(),
            MetricsPersistenceStatus::default()
        );
        let report = MetricsRegistry::report_from_event_log_path(
            &path,
            MetricsEventLogConfig::default(),
            100,
        );
        assert_eq!(report.events.len(), 21);
        assert_eq!(
            report.events.last().map(|event| event.name.as_str()),
            Some("shutdown.reopened")
        );
    }

    #[test]
    fn writer_failure_is_visible_after_flush() {
        let dir = tempfile::tempdir().expect("temp dir");
        let blocked_root = dir.path().join("blocked");
        fs::write(&blocked_root, b"not a directory").expect("blocking file");
        let metrics = MetricsRegistry::with_event_log(&blocked_root);
        metrics.record_event("failed.event", 1, MetricLabels::new());

        let status = metrics.flush_persistence();
        assert!(status.writer_failed);
        assert_eq!(status.dropped_events, 0);
    }

    #[test]
    fn segmented_event_log_prunes_closed_segments_but_keeps_active() {
        let dir = tempfile::tempdir().expect("temp dir");
        let metrics = MetricsRegistry::with_event_log_config(
            dir.path().join("events.jsonl"),
            MetricsEventLogConfig {
                segment_max_bytes: 180,
                total_max_bytes: 360,
                recent_read_max_bytes: 4096,
            },
        );

        for index in 0..30 {
            metrics.record_event("test.event", index, MetricLabels::new());
        }

        let status = metrics.flush_persistence();
        assert_eq!(status, MetricsPersistenceStatus::default());
        let manifest: MetricsEventLogManifest = serde_json::from_slice(
            &fs::read(dir.path().join(METRICS_MANIFEST_FILE_NAME)).expect("manifest should exist"),
        )
        .expect("manifest should parse");
        assert!(!manifest.segments.is_empty());
        assert!(
            manifest
                .segments
                .iter()
                .any(|segment| segment.name == manifest.active_segment
                    && segment.ended_unix_ms.is_none())
        );
        assert!(dir.path().join(&manifest.active_segment).exists());
        assert!(manifest.total_segment_bytes <= 360 || manifest.segments.len() == 1);
    }
}
