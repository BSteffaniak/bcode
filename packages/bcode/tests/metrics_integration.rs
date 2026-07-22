#![cfg(feature = "metrics")]

use bcode::telemetry::{SdkMetricsLayer, metric_names};
use bcode_metrics::MetricsRegistry;
use tracing_subscriber::layer::SubscriberExt as _;

#[test]
fn sdk_metrics_layer_records_stable_operational_metrics_without_sensitive_labels() {
    let registry = MetricsRegistry::in_memory();
    let layer = SdkMetricsLayer::new(registry.clone());
    let subscriber = tracing_subscriber::registry().with(layer);

    tracing::subscriber::with_default(subscriber, || {
        let request = tracing::info_span!(
            target: "bcode::sdk",
            "bcode.model_request",
            session_id = "sensitive-session",
            provider_id = "provider",
            model_id = "model",
            streaming = true,
        );
        let _request = request.enter();
        let tool = tracing::info_span!(
            target: "bcode::sdk",
            "bcode.tool_call",
            turn_id = "sensitive-turn",
            tool_call_id = "sensitive-call",
            tool_name = "search",
        );
        drop(tool.enter());
        tracing::info!(
            target: "bcode::sdk",
            event = "bcode.retry_scheduled",
            provider_id = "provider",
            model_id = "model",
            delay_ms = 17_u64,
        );
        tracing::info!(
            target: "bcode::sdk",
            event = "bcode.cache_lookup",
            provider_id = "provider",
            model_id = "model",
            cache_hit = true,
        );
        tracing::info!(
            target: "bcode::sdk",
            event = "bcode.usage",
            provider_id = "provider",
            input_tokens = 3_u64,
            output_tokens = 5_u64,
            total_tokens = 8_u64,
            cached_input_tokens = 1_u64,
            cache_write_input_tokens = 2_u64,
            reasoning_tokens = 4_u64,
            usage_available = true,
        );
        tracing::info!(
            target: "bcode::sdk",
            event = "bcode.cost_estimate",
            provider_id = "provider",
            model_id = "model",
            currency = "USD",
            total_micros = 41_u64,
            pricing_source = "provider_api",
            cost_available = true,
            estimated = true,
        );
        tracing::info!(
            target: "bcode::sdk",
            event = "bcode.rate_limit",
            limiter_id = "application",
            provider_id = "provider",
            model_id = "model",
            retry_at_unix = 123_u64,
            reset_available = true,
        );
        tracing::info!(
            target: "bcode::sdk",
            event = "bcode.error",
            error_origin = "provider",
            provider_error_category = "rate_limit",
            provider_error_code = "safe-code",
            request_id = "safe-request",
        );
        tracing::info!(
            target: "bcode::sdk",
            event = "bcode.cancellation",
            provider_id = "provider",
        );
    });

    let report = registry.report();
    assert_eq!(report.snapshot.counters[metric_names::MODEL_REQUESTS], 1);
    assert_eq!(report.snapshot.counters[metric_names::TOOL_CALLS], 1);
    assert_eq!(report.snapshot.counters[metric_names::RETRIES], 1);
    assert_eq!(report.snapshot.counters[metric_names::CACHE_LOOKUPS], 1);
    assert_eq!(report.snapshot.counters[metric_names::TOTAL_TOKENS], 8);
    assert_eq!(report.snapshot.counters[metric_names::COST_MICROS], 41);
    assert_eq!(report.snapshot.counters[metric_names::RATE_LIMITS], 2);
    assert_eq!(report.snapshot.counters[metric_names::ERRORS], 1);
    assert_eq!(report.snapshot.counters[metric_names::CANCELLATIONS], 1);
    assert_eq!(report.snapshot.gauges[metric_names::ACTIVE_STREAMS], 0);
    assert_eq!(
        report.snapshot.histograms[metric_names::RETRY_DELAY_MS].sum,
        17
    );
    assert!(
        report
            .snapshot
            .histograms
            .contains_key(metric_names::MODEL_REQUEST_LATENCY_MS)
    );
    assert!(
        report
            .snapshot
            .histograms
            .contains_key(metric_names::TOOL_CALL_LATENCY_MS)
    );
    for event in report.events {
        for forbidden in [
            "session_id",
            "turn_id",
            "tool_call_id",
            "prompt",
            "message",
            "cache_key",
            "request_id",
            "provider_error_code",
        ] {
            assert!(!event.labels.contains_key(forbidden), "leaked {forbidden}");
        }
    }
}

#[test]
fn sdk_metrics_layer_records_unavailable_usage_and_cost_honestly() {
    let registry = MetricsRegistry::in_memory();
    let subscriber = tracing_subscriber::registry().with(SdkMetricsLayer::new(registry.clone()));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(
            target: "bcode::sdk",
            event = "bcode.usage",
            provider_id = "provider",
            usage_available = false,
        );
        tracing::info!(
            target: "bcode::sdk",
            event = "bcode.cost_estimate",
            provider_id = "provider",
            model_id = "model",
            currency = "",
            total_micros = 0_u64,
            pricing_source = "unavailable",
            cost_available = false,
            estimated = true,
        );
    });
    let snapshot = registry.snapshot();
    assert_eq!(snapshot.counters[metric_names::USAGE_UNAVAILABLE], 1);
    assert_eq!(snapshot.counters[metric_names::COST_UNAVAILABLE], 1);
    assert!(!snapshot.counters.contains_key(metric_names::COST_MICROS));
}
