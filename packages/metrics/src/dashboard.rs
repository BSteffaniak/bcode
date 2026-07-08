//! Dashboard-oriented metrics aggregation for persisted and in-memory reports.

use crate::{
    MetricEvent, MetricKind, MetricLabels, MetricsAnalysis, MetricsReport, analyze_metrics_report,
    descriptors_from_events, snapshot_from_events,
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

/// Dashboard sort direction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricSortDirection {
    /// Smallest values first.
    Asc,
    /// Largest values first.
    #[default]
    Desc,
}

/// Field used to sort dashboard rows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricSortField {
    /// Metric name.
    Metric,
    /// Row group display value.
    Group,
    /// Event count.
    Count,
    /// Average value.
    Average,
    /// Maximum value.
    #[default]
    Max,
    /// Most recent event timestamp.
    LastSeen,
    /// Label value by key.
    Label(String),
}

/// Dashboard row sort.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricSort {
    /// Field to sort by.
    pub field: MetricSortField,
    /// Sort direction.
    pub direction: MetricSortDirection,
}

/// Metric event filter target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricFilterTarget {
    /// Metric name.
    Metric,
    /// Metric kind.
    Kind,
    /// Dashboard domain.
    Domain,
    /// Event integer value.
    Value,
    /// Label value by key.
    Label(String),
}

/// Metric event filter operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricFilterOp {
    /// Target equals value.
    Equals,
    /// Target does not equal value.
    NotEquals,
    /// Target contains value.
    Contains,
    /// Target exists.
    Exists,
    /// Target is missing.
    Missing,
    /// Numeric target is greater than value.
    GreaterThan,
    /// Numeric target is less than value.
    LessThan,
}

/// One dashboard event filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricFilter {
    /// Filter target.
    pub target: MetricFilterTarget,
    /// Filter operation.
    pub op: MetricFilterOp,
    /// Filter value when the operation needs one.
    #[serde(default)]
    pub value: Option<String>,
}

/// Dashboard event grouping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricGroupBy {
    /// Group by metric name.
    Metric,
    /// Group by metric name and full label set.
    #[default]
    MetricAndLabels,
    /// Group by a single label key.
    Label(String),
    /// Group by several label keys.
    Labels(Vec<String>),
    /// Group by dashboard domain.
    Domain,
}

/// Generic dashboard query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsDashboardQuery {
    /// Event filters.
    #[serde(default)]
    pub filters: Vec<MetricFilter>,
    /// Row sort.
    #[serde(default)]
    pub sort: MetricSort,
    /// Row grouping.
    #[serde(default)]
    pub group_by: MetricGroupBy,
    /// Text search across metric names and labels.
    #[serde(default)]
    pub search: Option<String>,
    /// Maximum rows to return per domain.
    #[serde(default = "default_query_limit")]
    pub limit: usize,
}

impl Default for MetricsDashboardQuery {
    fn default() -> Self {
        Self {
            filters: Vec::new(),
            sort: MetricSort::default(),
            group_by: MetricGroupBy::default(),
            search: None,
            limit: default_query_limit(),
        }
    }
}

/// Available label value facet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricFacetValue {
    /// Label value.
    pub value: String,
    /// Matching event count.
    pub count: usize,
}

/// Available label key facet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricFacet {
    /// Label key.
    pub key: String,
    /// Most common values for this key.
    pub values: Vec<MetricFacetValue>,
}

/// Result of applying a dashboard query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsDashboardQueryResult {
    /// Filtered report.
    pub report: MetricsReport,
    /// Dashboard built from the filtered report and query grouping/sorting.
    pub dashboard: MetricsDashboardData,
    /// Available facets in the filtered event set.
    pub facets: Vec<MetricFacet>,
    /// Events before filtering.
    pub total_events: usize,
    /// Events after filtering.
    pub filtered_events: usize,
}

const fn default_query_limit() -> usize {
    100
}

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
    /// Last event timestamp in Unix milliseconds.
    #[serde(default)]
    pub last_seen_unix_ms: u64,
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

/// Build dashboard data by applying a generic label/property query to a report.
#[must_use]
pub fn query_dashboard_report(
    report: &MetricsReport,
    query: &MetricsDashboardQuery,
) -> MetricsDashboardQueryResult {
    let events = report
        .events
        .iter()
        .filter(|event| query_matches_event(event, query))
        .cloned()
        .collect::<Vec<_>>();
    let filtered_report = MetricsReport {
        generated_at_unix_ms: report.generated_at_unix_ms,
        snapshot: snapshot_from_events(&events),
        descriptors: descriptors_from_events(&events),
        events,
    };
    let mut dashboard = dashboard_from_report(&filtered_report);
    apply_query_rows(&mut dashboard, query);
    MetricsDashboardQueryResult {
        facets: facets_from_events(&filtered_report.events),
        total_events: report.events.len(),
        filtered_events: filtered_report.events.len(),
        report: filtered_report,
        dashboard,
    }
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

fn query_matches_event(event: &MetricEvent, query: &MetricsDashboardQuery) -> bool {
    query
        .filters
        .iter()
        .all(|filter| filter_matches_event(event, filter))
        && query
            .search
            .as_ref()
            .is_none_or(|search| event_matches_search(event, search))
}

fn event_matches_search(event: &MetricEvent, search: &str) -> bool {
    let search = search.to_ascii_lowercase();
    event.name.to_ascii_lowercase().contains(&search)
        || event.labels.iter().any(|(key, value)| {
            key.to_ascii_lowercase().contains(&search)
                || value.to_ascii_lowercase().contains(&search)
        })
}

fn filter_matches_event(event: &MetricEvent, filter: &MetricFilter) -> bool {
    match &filter.target {
        MetricFilterTarget::Metric => compare_string(Some(event.name.as_str()), filter),
        MetricFilterTarget::Kind => {
            let kind = format!("{:?}", event.kind).to_ascii_lowercase();
            compare_string(Some(kind.as_str()), filter)
        }
        MetricFilterTarget::Domain => {
            let domain = format!("{:?}", metric_domain(&event.name)).to_ascii_lowercase();
            compare_string(Some(domain.as_str()), filter)
        }
        MetricFilterTarget::Label(key) => {
            compare_string(event.labels.get(key).map(String::as_str), filter)
        }
        MetricFilterTarget::Value => compare_number(event.value, filter),
    }
}

fn compare_string(actual: Option<&str>, filter: &MetricFilter) -> bool {
    match filter.op {
        MetricFilterOp::Exists => actual.is_some(),
        MetricFilterOp::Missing => actual.is_none(),
        MetricFilterOp::Equals => actual
            .zip(filter.value.as_ref())
            .is_some_and(|(actual, expected)| actual == expected),
        MetricFilterOp::NotEquals => actual
            .zip(filter.value.as_ref())
            .is_none_or(|(actual, expected)| actual != expected),
        MetricFilterOp::Contains => actual
            .zip(filter.value.as_ref())
            .is_some_and(|(actual, expected)| actual.contains(expected)),
        MetricFilterOp::GreaterThan | MetricFilterOp::LessThan => false,
    }
}

fn compare_number(actual: i64, filter: &MetricFilter) -> bool {
    let expected = filter
        .value
        .as_ref()
        .and_then(|value| value.parse::<i64>().ok());
    match (filter.op, expected) {
        (MetricFilterOp::Exists, _) => true,
        (MetricFilterOp::Equals, Some(expected)) => actual == expected,
        (MetricFilterOp::NotEquals, Some(expected)) => actual != expected,
        (MetricFilterOp::GreaterThan, Some(expected)) => actual > expected,
        (MetricFilterOp::LessThan, Some(expected)) => actual < expected,
        _ => false,
    }
}

fn apply_query_rows(dashboard: &mut MetricsDashboardData, query: &MetricsDashboardQuery) {
    for summary in &mut dashboard.domains {
        summary.rows = grouped_rows_for_query(&summary.rows, &query.group_by);
        sort_rows(&mut summary.rows, &query.sort);
        summary.rows.truncate(query.limit.max(1));
    }
}

fn grouped_rows_for_query(
    rows: &[MetricTableRow],
    group_by: &MetricGroupBy,
) -> Vec<MetricTableRow> {
    if matches!(group_by, MetricGroupBy::MetricAndLabels) {
        return rows.to_vec();
    }
    let mut grouped = BTreeMap::<(String, MetricLabels), RowAccumulator>::new();
    for row in rows {
        let (metric, labels) = row_group_key(row, group_by);
        let entry = grouped.entry((metric, labels)).or_default();
        entry.count = entry.count.saturating_add(row.count);
        entry.total = entry
            .total
            .saturating_add(row.average.saturating_mul(row.count));
        entry.max = entry.max.max(row.max);
        entry.last_seen_unix_ms = entry.last_seen_unix_ms.max(row.last_seen_unix_ms);
    }
    grouped
        .into_iter()
        .map(|((metric, labels), row)| MetricTableRow {
            group: label_summary(&labels),
            metric,
            count: row.count,
            average: row.total.checked_div(row.count).unwrap_or(0),
            max: row.max,
            last_seen_unix_ms: row.last_seen_unix_ms,
            labels,
        })
        .collect()
}

fn row_group_key(row: &MetricTableRow, group_by: &MetricGroupBy) -> (String, MetricLabels) {
    match group_by {
        MetricGroupBy::Metric | MetricGroupBy::MetricAndLabels => {
            (row.metric.clone(), MetricLabels::new())
        }
        MetricGroupBy::Label(key) => {
            let mut labels = MetricLabels::new();
            labels.insert(
                key.clone(),
                row.labels
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| "<missing>".to_owned()),
            );
            (key.clone(), labels)
        }
        MetricGroupBy::Labels(keys) => {
            let labels = keys
                .iter()
                .map(|key| {
                    (
                        key.clone(),
                        row.labels
                            .get(key)
                            .cloned()
                            .unwrap_or_else(|| "<missing>".to_owned()),
                    )
                })
                .collect();
            ("labels".to_owned(), labels)
        }
        MetricGroupBy::Domain => {
            let mut labels = MetricLabels::new();
            labels.insert(
                "domain".to_owned(),
                format!("{:?}", metric_domain(&row.metric)).to_ascii_lowercase(),
            );
            ("domain".to_owned(), labels)
        }
    }
}

fn sort_rows(rows: &mut [MetricTableRow], sort: &MetricSort) {
    rows.sort_by(|left, right| {
        let ordering = match &sort.field {
            MetricSortField::Metric => left.metric.cmp(&right.metric),
            MetricSortField::Group => left.group.cmp(&right.group),
            MetricSortField::Count => left.count.cmp(&right.count),
            MetricSortField::Average => left.average.cmp(&right.average),
            MetricSortField::Max => left.max.cmp(&right.max),
            MetricSortField::LastSeen => left.last_seen_unix_ms.cmp(&right.last_seen_unix_ms),
            MetricSortField::Label(key) => left.labels.get(key).cmp(&right.labels.get(key)),
        };
        match sort.direction {
            MetricSortDirection::Asc => ordering,
            MetricSortDirection::Desc => ordering.reverse(),
        }
    });
}

fn facets_from_events(events: &[MetricEvent]) -> Vec<MetricFacet> {
    let mut counts = BTreeMap::<String, BTreeMap<String, usize>>::new();
    for event in events {
        for (key, value) in &event.labels {
            *counts
                .entry(key.clone())
                .or_default()
                .entry(value.clone())
                .or_default() += 1;
        }
    }
    counts
        .into_iter()
        .map(|(key, values)| {
            let mut values = values
                .into_iter()
                .map(|(value, count)| MetricFacetValue { value, count })
                .collect::<Vec<_>>();
            values.sort_by_key(|value| std::cmp::Reverse(value.count));
            values.truncate(20);
            MetricFacet { key, values }
        })
        .collect()
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
        entry.last_seen_unix_ms = entry.last_seen_unix_ms.max(event.unix_ms);
    }
    let mut rows = grouped
        .into_iter()
        .map(|((metric, labels), row)| MetricTableRow {
            group: label_summary(&labels),
            metric,
            count: row.count,
            average: row.total.checked_div(row.count).unwrap_or(0),
            max: row.max,
            last_seen_unix_ms: row.last_seen_unix_ms,
            labels,
        })
        .collect::<Vec<_>>();
    sort_rows(&mut rows, &MetricSort::default());
    rows.truncate(default_query_limit());
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
    last_seen_unix_ms: u64,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, value: i64, labels: &[(&str, &str)]) -> MetricEvent {
        MetricEvent {
            unix_ms: u64::try_from(value).unwrap_or_default(),
            name: name.to_owned(),
            kind: MetricKind::Histogram,
            value,
            labels: labels
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        }
    }

    fn report() -> MetricsReport {
        let events = vec![
            event(
                "model.provider.duration_ms",
                10,
                &[("session_id", "a"), ("provider", "one")],
            ),
            event(
                "model.provider.duration_ms",
                30,
                &[("session_id", "b"), ("provider", "one")],
            ),
            event(
                "tool.exec.duration_ms",
                20,
                &[("session_id", "a"), ("tool", "shell")],
            ),
        ];
        MetricsReport {
            generated_at_unix_ms: 1,
            snapshot: snapshot_from_events(&events),
            descriptors: descriptors_from_events(&events),
            events,
        }
    }

    #[test]
    fn query_filters_by_any_label() {
        let query = MetricsDashboardQuery {
            filters: vec![MetricFilter {
                target: MetricFilterTarget::Label("session_id".to_owned()),
                op: MetricFilterOp::Equals,
                value: Some("a".to_owned()),
            }],
            ..MetricsDashboardQuery::default()
        };
        let result = query_dashboard_report(&report(), &query);
        assert_eq!(result.filtered_events, 2);
        assert!(
            result
                .report
                .events
                .iter()
                .all(|event| event.labels.get("session_id") == Some(&"a".to_owned()))
        );
    }

    #[test]
    fn query_groups_and_sorts_by_label() {
        let query = MetricsDashboardQuery {
            group_by: MetricGroupBy::Label("session_id".to_owned()),
            sort: MetricSort {
                field: MetricSortField::Count,
                direction: MetricSortDirection::Desc,
            },
            ..MetricsDashboardQuery::default()
        };
        let result = query_dashboard_report(&report(), &query);
        let rows = &result
            .dashboard
            .domains
            .iter()
            .find(|summary| summary.domain == MetricDomain::Overview)
            .expect("overview")
            .rows;
        assert_eq!(
            rows.first().and_then(|row| row.labels.get("session_id")),
            Some(&"a".to_owned())
        );
        assert_eq!(rows.first().map(|row| row.count), Some(2));
    }

    #[test]
    fn query_collects_label_facets() {
        let result = query_dashboard_report(&report(), &MetricsDashboardQuery::default());
        let session_facet = result
            .facets
            .iter()
            .find(|facet| facet.key == "session_id")
            .expect("session facet");
        assert!(
            session_facet
                .values
                .iter()
                .any(|value| value.value == "a" && value.count == 2)
        );
    }
}
