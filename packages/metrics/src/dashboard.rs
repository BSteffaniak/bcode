//! Dashboard-oriented metrics aggregation for persisted and in-memory reports.

use crate::{
    MetricEvent, MetricKind, MetricLabels, MetricsAnalysis, MetricsReport, analyze_metrics_report,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// High-level health level for dashboard cards and domain summaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricsHealth {
    /// No notable problems were detected.
    #[default]
    Good,
    /// One or more warning-level conditions were detected.
    Warning,
    /// One or more critical conditions were detected.
    Critical,
}

/// Dashboard domain grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricDomain {
    /// Summary across all domains.
    Overview,
    /// Model provider and request-build metrics.
    Provider,
    /// Tool execution and permission metrics.
    Tool,
    /// Plugin runtime metrics.
    Plugin,
    /// Session lifecycle and persistence metrics.
    Session,
    /// IPC request/response/event metrics.
    Ipc,
    /// Metrics system storage/retention metrics.
    Storage,
    /// Runtime work and uncategorized runtime metrics.
    Runtime,
    /// Uncategorized/raw metrics.
    Raw,
}

/// Dashboard-ready metrics data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsDashboardData {
    /// Report generation timestamp in Unix milliseconds.
    pub generated_at_unix_ms: u64,
    /// Overall health.
    pub health: MetricsHealth,
    /// Total recent events included in this dashboard.
    pub event_count: usize,
    /// Derived analysis.
    pub analysis: MetricsAnalysis,
    /// Per-domain summaries.
    pub domains: Vec<MetricDomainSummary>,
}

/// Summary for one dashboard domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricDomainSummary {
    /// Domain represented by this summary.
    pub domain: MetricDomain,
    /// Domain health.
    pub health: MetricsHealth,
    /// KPI cards.
    pub cards: Vec<MetricCard>,
    /// Timeline series for sparklines.
    pub series: Vec<MetricSeries>,
    /// Main table rows.
    pub rows: Vec<MetricTableRow>,
    /// Actionable recommendations.
    pub recommendations: Vec<MetricRecommendation>,
}

/// Small KPI card shown at the top of dashboard tabs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricCard {
    /// Card title.
    pub title: String,
    /// Primary display value.
    pub value: String,
    /// Secondary display value.
    pub detail: String,
    /// Card health.
    pub health: MetricsHealth,
}

/// Sparkline/bar-chart series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricSeries {
    /// Series title.
    pub title: String,
    /// Display unit.
    pub unit: String,
    /// Series points.
    pub points: Vec<u64>,
}

/// Dashboard table row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricTableRow {
    /// Metric name.
    pub metric: String,
    /// Row label/group.
    pub group: String,
    /// Sample count.
    pub count: u64,
    /// Average value.
    pub average: u64,
    /// Maximum value.
    pub max: u64,
    /// Labels represented by this row.
    pub labels: MetricLabels,
}

/// Actionable dashboard recommendation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricRecommendation {
    /// Recommendation severity.
    pub health: MetricsHealth,
    /// Short title.
    pub title: String,
    /// Detail text.
    pub detail: String,
    /// Related metric.
    pub metric: String,
    /// Related labels.
    pub labels: MetricLabels,
}

/// Build dashboard data from a metrics report.
#[must_use]
pub fn dashboard_from_report(report: &MetricsReport) -> MetricsDashboardData {
    let analysis = analyze_metrics_report(report);
    let domains = [
        MetricDomain::Overview,
        MetricDomain::Provider,
        MetricDomain::Tool,
        MetricDomain::Plugin,
        MetricDomain::Session,
        MetricDomain::Ipc,
        MetricDomain::Storage,
        MetricDomain::Runtime,
        MetricDomain::Raw,
    ]
    .into_iter()
    .map(|domain| summarize_domain(report, &analysis, domain))
    .collect::<Vec<_>>();
    let health = if analysis
        .anomalies
        .iter()
        .any(|anomaly| anomaly.severity == "critical")
    {
        MetricsHealth::Critical
    } else if analysis.anomalies.is_empty() {
        MetricsHealth::Good
    } else {
        MetricsHealth::Warning
    };
    MetricsDashboardData {
        generated_at_unix_ms: report.generated_at_unix_ms,
        health,
        event_count: report.events.len(),
        analysis,
        domains,
    }
}

fn summarize_domain(
    report: &MetricsReport,
    analysis: &MetricsAnalysis,
    domain: MetricDomain,
) -> MetricDomainSummary {
    let events = domain_events(report, domain).collect::<Vec<_>>();
    let rows = table_rows(&events);
    let error_count = events
        .iter()
        .filter(|event| event.name.ends_with("errors_total") && event.value > 0)
        .count();
    let slowest = rows
        .iter()
        .filter(|row| is_duration_metric(&row.metric))
        .map(|row| row.max)
        .max()
        .unwrap_or(0);
    let health = if error_count > 0 || slowest >= 30_000 {
        MetricsHealth::Warning
    } else {
        MetricsHealth::Good
    };
    let cards = vec![
        MetricCard {
            title: "Events".to_owned(),
            value: events.len().to_string(),
            detail: "recent samples".to_owned(),
            health: MetricsHealth::Good,
        },
        MetricCard {
            title: "Errors".to_owned(),
            value: error_count.to_string(),
            detail: "recent error deltas".to_owned(),
            health: if error_count == 0 {
                MetricsHealth::Good
            } else {
                MetricsHealth::Warning
            },
        },
        MetricCard {
            title: "Slowest".to_owned(),
            value: if slowest == 0 {
                "none".to_owned()
            } else {
                format_ms(slowest)
            },
            detail: "max sample".to_owned(),
            health: if slowest >= 10_000 {
                MetricsHealth::Warning
            } else {
                MetricsHealth::Good
            },
        },
        MetricCard {
            title: "Groups".to_owned(),
            value: rows.len().to_string(),
            detail: "metric + label sets".to_owned(),
            health: MetricsHealth::Good,
        },
    ];
    let mut recommendations = recommendations_for_rows(&rows);
    if domain == MetricDomain::Overview {
        recommendations.extend(analysis.anomalies.iter().take(8).map(|anomaly| {
            MetricRecommendation {
                health: MetricsHealth::Warning,
                title: anomaly.code.clone(),
                detail: anomaly.message.clone(),
                metric: anomaly.metric.clone(),
                labels: anomaly.labels.clone(),
            }
        }));
    }
    MetricDomainSummary {
        domain,
        health,
        cards,
        series: series_for_domain(&events),
        rows,
        recommendations,
    }
}

fn domain_events(
    report: &MetricsReport,
    domain: MetricDomain,
) -> impl Iterator<Item = &MetricEvent> {
    report.events.iter().filter(move |event| {
        domain == MetricDomain::Overview || metric_domain(&event.name) == domain
    })
}

/// Classify one metric name into a dashboard domain.
#[must_use]
pub fn metric_domain(name: &str) -> MetricDomain {
    if name.starts_with("model.") {
        MetricDomain::Provider
    } else if name.starts_with("tool.") {
        MetricDomain::Tool
    } else if name.starts_with("plugin.") {
        MetricDomain::Plugin
    } else if name.starts_with("session.") {
        MetricDomain::Session
    } else if name.starts_with("ipc.") {
        MetricDomain::Ipc
    } else if name.starts_with("metrics.") {
        MetricDomain::Storage
    } else if name.starts_with("runtime") {
        MetricDomain::Runtime
    } else {
        MetricDomain::Raw
    }
}

fn table_rows(events: &[&MetricEvent]) -> Vec<MetricTableRow> {
    let mut grouped = BTreeMap::<(String, MetricLabels), RowAccumulator>::new();
    for event in events {
        if event.value < 0 {
            continue;
        }
        let value = u64::try_from(event.value).unwrap_or(u64::MAX);
        let entry = grouped
            .entry((event.name.clone(), event.labels.clone()))
            .or_default();
        entry.count = entry.count.saturating_add(1);
        entry.total = entry.total.saturating_add(value);
        entry.max = entry.max.max(value);
    }
    let mut rows = grouped
        .into_iter()
        .map(|((metric, labels), row)| MetricTableRow {
            group: label_summary(&labels),
            metric,
            count: row.count,
            average: row.total.checked_div(row.count).unwrap_or(0),
            max: row.max,
            labels,
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|row| std::cmp::Reverse((row.max, row.average, row.count)));
    rows.truncate(100);
    rows
}

fn series_for_domain(events: &[&MetricEvent]) -> Vec<MetricSeries> {
    let histogram_events = events
        .iter()
        .filter(|event| matches!(event.kind, MetricKind::Histogram) && event.value >= 0)
        .collect::<Vec<_>>();
    let points = histogram_events
        .iter()
        .rev()
        .take(80)
        .map(|event| u64::try_from(event.value).unwrap_or_default())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    vec![MetricSeries {
        title: "Recent histogram samples".to_owned(),
        unit: "value".to_owned(),
        points,
    }]
}

fn recommendations_for_rows(rows: &[MetricTableRow]) -> Vec<MetricRecommendation> {
    rows.iter()
        .filter(|row| {
            (is_duration_metric(&row.metric) && row.max >= 10_000)
                || row.metric.ends_with("errors_total")
                || (is_bytes_metric(&row.metric) && row.max >= 10 * 1024 * 1024)
        })
        .take(8)
        .map(|row| MetricRecommendation {
            health: MetricsHealth::Warning,
            title: if row.metric.ends_with("errors_total") {
                "Errors observed".to_owned()
            } else if is_bytes_metric(&row.metric) {
                "Large payload".to_owned()
            } else {
                "Slow metric group".to_owned()
            },
            detail: format!(
                "{} max={} avg={} count={} labels={}",
                row.metric,
                format_dashboard_value(&row.metric, row.max),
                format_dashboard_value(&row.metric, row.average),
                row.count,
                row.group
            ),
            metric: row.metric.clone(),
            labels: row.labels.clone(),
        })
        .collect()
}

fn label_summary(labels: &MetricLabels) -> String {
    if labels.is_empty() {
        return "global".to_owned();
    }
    labels
        .iter()
        .take(4)
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_dashboard_value(metric: &str, value: u64) -> String {
    if is_duration_metric(metric) {
        format_ms(value)
    } else if is_bytes_metric(metric) {
        format_bytes(value)
    } else {
        value.to_string()
    }
}

fn is_duration_metric(metric: &str) -> bool {
    metric.ends_with("duration_ms")
}

fn is_bytes_metric(metric: &str) -> bool {
    metric.ends_with("bytes")
        || metric.ends_with("payload_bytes")
        || metric.ends_with("output_bytes")
}

fn format_bytes(value: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    if value >= MIB {
        format!("{}.{}MiB", value / MIB, (value % MIB) / (MIB / 10))
    } else if value >= KIB {
        format!("{}.{}KiB", value / KIB, (value % KIB) / (KIB / 10))
    } else {
        format!("{value}B")
    }
}

fn format_ms(value: u64) -> String {
    if value >= 1_000 {
        format!("{}.{}s", value / 1_000, (value % 1_000) / 100)
    } else {
        format!("{value}ms")
    }
}

#[derive(Debug, Default)]
struct RowAccumulator {
    count: u64,
    total: u64,
    max: u64,
}

/// Return label keys that appear to be high-cardinality in the report.
#[must_use]
pub fn high_cardinality_label_keys(
    report: &MetricsReport,
    threshold: usize,
) -> BTreeMap<String, usize> {
    let mut values = BTreeMap::<String, BTreeSet<String>>::new();
    for event in &report.events {
        for (key, value) in &event.labels {
            values.entry(key.clone()).or_default().insert(value.clone());
        }
    }
    values
        .into_iter()
        .filter_map(|(key, values)| {
            let count = values.len();
            (count >= threshold).then_some((key, count))
        })
        .collect()
}
