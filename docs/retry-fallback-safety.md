# Retry and fallback safety

Provider attempt recovery is owned by `ProviderRoundPlanner` and occurs inside one canonical model
round. Whole-turn timeout and cancellation bound planner work, backoff sleep, provider start/poll,
and cleanup.

## Retry policy

`RetryPolicy` retries legacy pre-output `ProviderInvocation` failures for compatibility. Structured provider errors retry only when `ProviderError.retryable` is true or when an explicit
provider/catalog/user rule classifies the failure as recoverable. Ephemeral retries are indefinite
by default. Their exponential delay grows to the configured `max_delay` (10 minutes by default),
then remains at that delay until success or cancellation. Numeric `max_retries` values preserve
finite retry behavior; set `retry_forever = false` alongside a limit when overriding an inherited
indefinite rule. Provider `ProviderRetryHint.retry_after_ms` and `retry_at_unix` establish a minimum
delay and are capped by configured `max_delay`. Optional jitter is deterministic from
provider/model/attempt identity and bounded by the remaining delay cap, avoiding global RNG/state
while preventing identical request schedules.

Cancellation, host timeout, tool, permission, middleware, cache, validation, and application errors
are terminal. Once any model-visible text, reasoning, or tool-call output has been emitted, a later
provider failure becomes `RuntimeError::ProviderAfterOutput` and cannot be retried or routed. This
prevents duplicated visible stream effects.

## Fallback policy

`FallbackPolicy` changes provider/model only for transport failures or typed categories where another
provider/model can reasonably help: rate limit, network, timeout, model not found, unsupported
feature, provider internal, and overloaded. Auth, config, invalid-request, context-length, and
cancelled failures remain terminal and actionable rather than being hidden by routing.

Fallback selectors are ordered and bounded by their finite list. Fallback never runs as an
unbounded implicit policy; only errors classified as ephemeral use indefinite retries by default.

## Tool side effects

Provider retries are per model round. Tool execution occurs only after a provider round has
successfully completed with tool calls. If the following continuation provider round fails before
visible output, that continuation request may retry with the already-recorded tool result; the tool
batch itself is not executed again. Tests cover this with an `ExecuteProcess`-classified tool and
prove exactly one invocation. Provider failures after visible output are terminal, so retries cannot
replay tool-call output either.

Applications that need idempotent retry across process loss or external transport replay must use
their own tool idempotency keys; Bcode does not falsely infer external side-effect safety.
