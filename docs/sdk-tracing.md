# Stable SDK tracing contract

Bcode emits `tracing` spans and events under target `bcode::sdk`. The core SDK depends only on
`tracing`; it does not install a subscriber or require OpenTelemetry, metrics, logging storage, or a
particular exporter. Applications own filtering, sampling, redaction, and export.

Names and fields in this document are the stable observability contract. Additional fields may be
added compatibly. Prompt/message text, provider extension payloads, tool arguments/results,
credentials, cache-key source material, and session transcript content are not recorded.

## Spans

* `bcode.model_request`: one public buffered or streaming model request. Fields: `session_id`,
  `provider_id`, `model_id`, and `streaming`.
* `bcode.session_turn`: stateful session generation/regeneration wrapper. Fields: `session_id`,
  `provider_id`, `model_id`, and `operation` (`generate` or `regenerate`).
* `bcode.agent_turn`: canonical provider/tool loop. Fields: `session_id`, `turn_id`, `provider_id`,
  `model_id`, and `streaming`.
* `bcode.provider_round`: one model round, including planner retries. Fields: `turn_id`,
  `provider_id`, `model_id`, and zero-based `round`.
* `bcode.provider_operation`: provider lifecycle operation. Fields: `turn_id`, `provider_id`,
  `model_id` where available, and `operation` (`start` or `poll`).
* `bcode.tool_batch`: one admitted provider tool batch. Fields: `turn_id`, `provider_round`,
  `batch_size`, and `parallel`.
* `bcode.tool_call`: one canonical tool invocation. Fields: `turn_id`, `tool_call_id`, and
  `tool_name`.

Runtime-created streaming tasks explicitly inherit the active request span. Concurrent tool calls
receive separate child spans and therefore remain correlatable even when completion order differs.

## Events

Events use the `event` field as their stable identity:

* `bcode.retry_scheduled`: `turn_id`, `provider_id`, `model_id`, `round`, `attempt`, and `delay_ms`.
* `bcode.cache_lookup`: `cache_hit`, `provider_id`, and `model_id`.
* `bcode.cache_store`: `provider_id` and `model_id`.
* `bcode.cache_bypass`: `provider_id` and `model_id`.
* `bcode.usage`: provider-reported `input_tokens`, `output_tokens`, `total_tokens`,
  `cached_input_tokens`, `cache_write_input_tokens`, `reasoning_tokens`, and `usage_available`.
* `bcode.cost_estimate`: `provider_id`, `model_id`, ISO currency, cost `total_micros`,
  `pricing_source`, `cost_available`, and `estimated = true`. Cost is never presented as billed.
* `bcode.rate_limit`: application limiter ID plus provider/model identity and labeled reset
  availability; denial reasons are deliberately excluded.
* `bcode.error`: secret-safe `error_origin` plus allowlisted provider error category/code/request ID
  or tool name where available; error messages and payloads are excluded.
* `bcode.cancellation`: provider or scheduler cancellation correlation and bounded cancellation
  counts where applicable.

Existing lower-level debug diagnostics remain implementation detail unless listed here. Standard
span ancestry correlates request, session, provider, tool, retry, cache, and cancellation activity.
No global IDs or exporter-specific context are fabricated by the SDK.

## Optional adapters

The lean SDK has no OpenTelemetry or metrics dependency. Applications opt in explicitly:

* Feature `opentelemetry` exposes `bcode::telemetry::opentelemetry_layer()`. Attach an
  application-owned OpenTelemetry tracer with `.with_tracer(...)`, then compose the layer with the
  application's `tracing_subscriber` registry. Exporters, resources, sampling, propagation, and
  shutdown remain application responsibilities.
* Feature `metrics` exposes `bcode::telemetry::SdkMetricsLayer` and stable names in
  `bcode::telemetry::metric_names`. Compose the layer with a subscriber and pass an
  application-owned `bcode_metrics::MetricsRegistry`.

The metrics adapter derives request/provider/tool latency, active streams, retries and delay,
application/provider rate limits, cache hit/miss/store/bypass, cancellations, structured errors,
provider-reported token buckets, and estimated cost micros. Labels are intentionally bounded to
provider/model/operation/tool name/error category/limiter/currency/pricing source/streaming status.
Prompts, transcript content, tool inputs/results, provider extensions, cache keys, session/turn IDs,
error messages, and credentials are never metric labels. Cost metrics are labeled `estimated` and
include pricing source and currency; unavailable usage or pricing increments explicit unavailable
counters rather than fabricating zero spend.

A deterministic subscriber test validates every documented span/event, required correlation field,
stream-task span propagation, retry/cache behavior, session wrapping, tool IDs, and cancellation.
