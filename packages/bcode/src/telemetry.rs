//! Optional SDK telemetry adapters.
//!
//! Core SDK instrumentation always uses the stable `bcode::sdk` [`tracing`] contract. The
//! adapters in this module are compiled only when their matching crate feature is enabled, so
//! lean SDK users do not pay for metrics or OpenTelemetry dependencies.

#[cfg(feature = "opentelemetry")]
#[doc = "Create a `tracing-opentelemetry` layer to connect the SDK's stable spans and events to an application-owned tracer."]
#[doc = "Call `.with_tracer(...)` on the returned layer, then compose it with the application's tracing subscriber."]
pub use tracing_opentelemetry::layer as opentelemetry_layer;

#[cfg(feature = "metrics")]
mod metrics {
    use bcode_metrics::{MetricLabels, MetricsRegistry};
    use std::collections::BTreeMap;
    use std::fmt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;
    use tracing::field::{Field, Visit};
    use tracing::{Event, Subscriber};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::Context;
    use tracing_subscriber::registry::LookupSpan;

    /// Stable SDK metric names emitted by [`SdkMetricsLayer`].
    pub mod names {
        /// Completed model requests.
        pub const MODEL_REQUESTS: &str = "bcode.sdk.model_requests";
        /// Model request wall-clock latency in milliseconds.
        pub const MODEL_REQUEST_LATENCY_MS: &str = "bcode.sdk.model_request.latency_ms";
        /// Provider lifecycle operation latency in milliseconds.
        pub const PROVIDER_OPERATION_LATENCY_MS: &str = "bcode.sdk.provider_operation.latency_ms";
        /// Agent provider/tool loop latency in milliseconds.
        pub const AGENT_TURN_LATENCY_MS: &str = "bcode.sdk.agent_turn.latency_ms";
        /// Tool invocation count.
        pub const TOOL_CALLS: &str = "bcode.sdk.tool_calls";
        /// Tool invocation latency in milliseconds.
        pub const TOOL_CALL_LATENCY_MS: &str = "bcode.sdk.tool_call.latency_ms";
        /// Active public streaming model requests.
        pub const ACTIVE_STREAMS: &str = "bcode.sdk.active_streams";
        /// Provider/planner retry count.
        pub const RETRIES: &str = "bcode.sdk.retries";
        /// Scheduled retry delay in milliseconds.
        pub const RETRY_DELAY_MS: &str = "bcode.sdk.retry_delay_ms";
        /// Application or provider rate-limit count.
        pub const RATE_LIMITS: &str = "bcode.sdk.rate_limits";
        /// Cache lookup count.
        pub const CACHE_LOOKUPS: &str = "bcode.sdk.cache.lookups";
        /// Cache store count.
        pub const CACHE_STORES: &str = "bcode.sdk.cache.stores";
        /// Cache bypass count.
        pub const CACHE_BYPASSES: &str = "bcode.sdk.cache.bypasses";
        /// Cancellation count.
        pub const CANCELLATIONS: &str = "bcode.sdk.cancellations";
        /// Structured SDK/provider error count.
        pub const ERRORS: &str = "bcode.sdk.errors";
        /// Provider-reported input tokens.
        pub const INPUT_TOKENS: &str = "bcode.sdk.tokens.input";
        /// Provider-reported output tokens.
        pub const OUTPUT_TOKENS: &str = "bcode.sdk.tokens.output";
        /// Provider-reported or derived total tokens.
        pub const TOTAL_TOKENS: &str = "bcode.sdk.tokens.total";
        /// Provider-reported cached input tokens.
        pub const CACHED_INPUT_TOKENS: &str = "bcode.sdk.tokens.cached_input";
        /// Provider-reported cache-write input tokens.
        pub const CACHE_WRITE_INPUT_TOKENS: &str = "bcode.sdk.tokens.cache_write_input";
        /// Provider-reported reasoning tokens.
        pub const REASONING_TOKENS: &str = "bcode.sdk.tokens.reasoning";
        /// Requests for which provider token usage was unavailable.
        pub const USAGE_UNAVAILABLE: &str = "bcode.sdk.usage_unavailable";
        /// Estimated monetary cost in micros of the labeled currency.
        pub const COST_MICROS: &str = "bcode.sdk.cost_micros";
        /// Requests for which no non-zero cost estimate was available.
        pub const COST_UNAVAILABLE: &str = "bcode.sdk.cost_unavailable";
    }

    /// `tracing_subscriber` layer that derives bounded, secret-safe operational metrics from the
    /// stable SDK tracing contract.
    ///
    /// The layer deliberately excludes prompts, message content, tool inputs/results, provider
    /// extensions, cache keys, session IDs, turn IDs, and provider error messages from labels.
    /// Applications own registry persistence/export and subscriber filtering.
    #[derive(Debug, Clone)]
    pub struct SdkMetricsLayer {
        registry: MetricsRegistry,
        active_streams: Arc<AtomicU64>,
    }

    impl SdkMetricsLayer {
        /// Create an SDK metrics layer writing into an application-owned registry.
        #[must_use]
        pub fn new(registry: MetricsRegistry) -> Self {
            Self {
                registry,
                active_streams: Arc::new(AtomicU64::new(0)),
            }
        }

        /// Return the registry receiving derived SDK metrics.
        #[must_use]
        pub const fn registry(&self) -> &MetricsRegistry {
            &self.registry
        }
    }

    #[derive(Debug)]
    struct SpanMetrics {
        name: &'static str,
        started: Instant,
        fields: Fields,
        active_stream: bool,
    }

    #[derive(Debug, Default, Clone)]
    struct Fields(BTreeMap<String, String>);

    impl Fields {
        fn get(&self, key: &str) -> Option<&str> {
            self.0.get(key).map(String::as_str)
        }

        fn bool(&self, key: &str) -> bool {
            self.get(key) == Some("true")
        }

        fn u64(&self, key: &str) -> Option<u64> {
            self.get(key)?.parse().ok()
        }

        fn merge(&mut self, other: &Self) {
            self.0.extend(other.0.clone());
        }
    }

    impl Visit for Fields {
        fn record_bool(&mut self, field: &Field, value: bool) {
            self.0.insert(field.name().to_owned(), value.to_string());
        }

        fn record_i64(&mut self, field: &Field, value: i64) {
            self.0.insert(field.name().to_owned(), value.to_string());
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name().to_owned(), value.to_string());
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            let value = format!("{value:?}");
            self.0.insert(
                field.name().to_owned(),
                value
                    .strip_prefix('"')
                    .and_then(|value| value.strip_suffix('"'))
                    .unwrap_or(&value)
                    .to_owned(),
            );
        }
    }

    impl<S> Layer<S> for SdkMetricsLayer
    where
        S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    {
        fn on_new_span(
            &self,
            attributes: &tracing::span::Attributes<'_>,
            id: &tracing::Id,
            context: Context<'_, S>,
        ) {
            if attributes.metadata().target() != "bcode::sdk" {
                return;
            }
            let mut fields = Fields::default();
            attributes.record(&mut fields);
            let name = attributes.metadata().name();
            let active_stream = name == "bcode.model_request" && fields.bool("streaming");
            if active_stream {
                let active = self.active_streams.fetch_add(1, Ordering::AcqRel) + 1;
                self.registry.set_gauge(
                    names::ACTIVE_STREAMS,
                    i64::try_from(active).unwrap_or(i64::MAX),
                );
            }
            if let Some(span) = context.span(id) {
                span.extensions_mut().insert(SpanMetrics {
                    name,
                    started: Instant::now(),
                    fields,
                    active_stream,
                });
            }
        }

        fn on_record(
            &self,
            id: &tracing::Id,
            values: &tracing::span::Record<'_>,
            context: Context<'_, S>,
        ) {
            let Some(span) = context.span(id) else {
                return;
            };
            let mut fields = Fields::default();
            values.record(&mut fields);
            if let Some(metrics) = span.extensions_mut().get_mut::<SpanMetrics>() {
                metrics.fields.merge(&fields);
            }
        }

        fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
            if event.metadata().target() != "bcode::sdk" {
                return;
            }
            let mut fields = Fields::default();
            if let Some(scope) = context.event_scope(event) {
                for span in scope.from_root() {
                    if let Some(metrics) = span.extensions().get::<SpanMetrics>() {
                        fields.merge(&metrics.fields);
                    }
                }
            }
            event.record(&mut fields);
            self.record_event(&fields);
        }

        fn on_close(&self, id: tracing::Id, context: Context<'_, S>) {
            let Some(span) = context.span(&id) else {
                return;
            };
            let Some(metrics) = span.extensions_mut().remove::<SpanMetrics>() else {
                return;
            };
            let elapsed = duration_millis(metrics.started.elapsed());
            let labels = labels(&metrics.fields);
            match metrics.name {
                "bcode.model_request" => {
                    self.registry
                        .add_counter_with_labels(names::MODEL_REQUESTS, 1, labels.clone());
                    self.registry.record_histogram_with_labels(
                        names::MODEL_REQUEST_LATENCY_MS,
                        elapsed,
                        labels,
                    );
                }
                "bcode.provider_operation" => self.registry.record_histogram_with_labels(
                    names::PROVIDER_OPERATION_LATENCY_MS,
                    elapsed,
                    labels,
                ),
                "bcode.agent_turn" => self.registry.record_histogram_with_labels(
                    names::AGENT_TURN_LATENCY_MS,
                    elapsed,
                    labels,
                ),
                "bcode.tool_call" => {
                    self.registry
                        .add_counter_with_labels(names::TOOL_CALLS, 1, labels.clone());
                    self.registry.record_histogram_with_labels(
                        names::TOOL_CALL_LATENCY_MS,
                        elapsed,
                        labels,
                    );
                }
                _ => {}
            }
            if metrics.active_stream {
                let active = self
                    .active_streams
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                        Some(active.saturating_sub(1))
                    })
                    .unwrap_or_default()
                    .saturating_sub(1);
                self.registry.set_gauge(
                    names::ACTIVE_STREAMS,
                    i64::try_from(active).unwrap_or(i64::MAX),
                );
            }
        }
    }

    impl SdkMetricsLayer {
        fn record_event(&self, fields: &Fields) {
            let Some(event) = fields.get("event") else {
                return;
            };
            let labels = labels(fields);
            match event {
                "bcode.retry_scheduled" => {
                    self.registry
                        .add_counter_with_labels(names::RETRIES, 1, labels.clone());
                    if let Some(delay) = fields.u64("delay_ms") {
                        self.registry.record_histogram_with_labels(
                            names::RETRY_DELAY_MS,
                            delay,
                            labels,
                        );
                    }
                }
                "bcode.cache_lookup" => {
                    let mut labels = labels;
                    labels.insert(
                        "cache_status".to_owned(),
                        if fields.bool("cache_hit") {
                            "hit"
                        } else {
                            "miss"
                        }
                        .to_owned(),
                    );
                    self.registry
                        .add_counter_with_labels(names::CACHE_LOOKUPS, 1, labels);
                }
                "bcode.cache_store" => {
                    self.registry
                        .add_counter_with_labels(names::CACHE_STORES, 1, labels);
                }
                "bcode.cache_bypass" => {
                    self.registry
                        .add_counter_with_labels(names::CACHE_BYPASSES, 1, labels);
                }
                "bcode.cancellation" => {
                    self.registry
                        .add_counter_with_labels(names::CANCELLATIONS, 1, labels);
                }
                "bcode.error" => {
                    self.registry
                        .add_counter_with_labels(names::ERRORS, 1, labels.clone());
                    if fields.get("provider_error_category") == Some("rate_limit") {
                        self.registry
                            .add_counter_with_labels(names::RATE_LIMITS, 1, labels);
                    }
                }
                "bcode.rate_limit" => {
                    self.registry
                        .add_counter_with_labels(names::RATE_LIMITS, 1, labels);
                }
                "bcode.usage" => self.record_usage(fields, labels),
                "bcode.cost_estimate" => {
                    if fields.bool("cost_available") {
                        self.registry.add_counter_with_labels(
                            names::COST_MICROS,
                            fields.u64("total_micros").unwrap_or_default(),
                            labels,
                        );
                    } else {
                        self.registry
                            .add_counter_with_labels(names::COST_UNAVAILABLE, 1, labels);
                    }
                }
                _ => {}
            }
        }

        fn record_usage(&self, fields: &Fields, labels: MetricLabels) {
            if !fields.bool("usage_available") {
                self.registry
                    .add_counter_with_labels(names::USAGE_UNAVAILABLE, 1, labels);
                return;
            }
            for (name, field) in [
                (names::INPUT_TOKENS, "input_tokens"),
                (names::OUTPUT_TOKENS, "output_tokens"),
                (names::TOTAL_TOKENS, "total_tokens"),
                (names::CACHED_INPUT_TOKENS, "cached_input_tokens"),
                (names::CACHE_WRITE_INPUT_TOKENS, "cache_write_input_tokens"),
                (names::REASONING_TOKENS, "reasoning_tokens"),
            ] {
                self.registry.add_counter_with_labels(
                    name,
                    fields.u64(field).unwrap_or_default(),
                    labels.clone(),
                );
            }
        }
    }

    fn labels(fields: &Fields) -> MetricLabels {
        let mut labels = MetricLabels::new();
        for key in [
            "provider_id",
            "model_id",
            "operation",
            "tool_name",
            "error_origin",
            "provider_error_category",
            "limiter_id",
            "currency",
            "pricing_source",
            "estimated",
            "streaming",
        ] {
            if let Some(value) = fields.get(key).filter(|value| !value.is_empty()) {
                labels.insert(key.to_owned(), value.to_owned());
            }
        }
        labels
    }

    fn duration_millis(duration: std::time::Duration) -> u64 {
        u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
    }
}

#[cfg(feature = "metrics")]
pub use metrics::SdkMetricsLayer;
#[cfg(feature = "metrics")]
pub use metrics::names as metric_names;
