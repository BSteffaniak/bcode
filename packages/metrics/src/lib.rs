#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::significant_drop_tightening)]

//! Lightweight in-process metrics for Bcode runtime diagnostics.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const HISTOGRAM_BUCKETS_MS: &[u64] = &[
    1, 2, 5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000,
];

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
    /// Increment a counter by one.
    pub fn increment_counter(&self, key: impl Into<String>) {
        self.add_counter(key, 1);
    }

    /// Add `value` to a counter.
    pub fn add_counter(&self, key: impl Into<String>, value: u64) {
        let mut state = self.inner.lock().expect("metrics registry lock poisoned");
        let counter = state.counters.entry(key.into()).or_default();
        *counter = counter.saturating_add(value);
    }

    /// Set a gauge value.
    pub fn set_gauge(&self, key: impl Into<String>, value: i64) {
        let mut state = self.inner.lock().expect("metrics registry lock poisoned");
        state.gauges.insert(key.into(), value);
    }

    /// Record a histogram sample.
    pub fn record_histogram(&self, key: impl Into<String>, value: u64) {
        let mut state = self.inner.lock().expect("metrics registry lock poisoned");
        state
            .histograms
            .entry(key.into())
            .or_default()
            .record(value);
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
        MetricsSnapshot {
            counters: state.counters.clone(),
            gauges: state.gauges.clone(),
            histograms: state
                .histograms
                .iter()
                .map(|(key, histogram)| (key.clone(), histogram.snapshot()))
                .collect(),
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
}
