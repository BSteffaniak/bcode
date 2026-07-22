# Retry and fallback safety

Provider attempt recovery is owned by `ProviderRoundPlanner` and occurs inside one canonical model
round. Whole-turn timeout and cancellation bound planner work, backoff sleep, provider start/poll,
and cleanup.

## Retry policy

`RetryPolicy` retries legacy pre-output `ProviderInvocation` failures for compatibility. Structured
provider errors retry only when `ProviderError.retryable` is true. Provider
`ProviderRetryHint.retry_after_ms` and `retry_at_unix` establish a minimum delay; exponential
application backoff and the provider hint are capped by configured `max_delay`. Optional jitter is
deterministic from provider/model/attempt identity and bounded by the remaining delay cap, avoiding
global RNG/state while preventing identical request schedules.

Cancellation, host timeout, tool, permission, middleware, cache, validation, and application errors
are terminal. Once any model-visible text, reasoning, or tool-call output has been emitted, a later
provider failure becomes `RuntimeError::ProviderAfterOutput` and cannot be retried or routed. This
prevents duplicated visible stream effects.

## Fallback policy

`FallbackPolicy` changes provider/model only for transport failures or typed categories where another
provider/model can reasonably help: rate limit, network, timeout, model not found, unsupported
feature, provider internal, and overloaded. Auth, config, invalid-request, context-length, and
cancelled failures remain terminal and actionable rather than being hidden by routing.

Fallback selectors are ordered and bounded by their finite list. Retry and fallback never run as an
unbounded implicit policy.

## Tool side effects

Provider retries are per model round. Tool execution occurs only after a provider round has
successfully completed with tool calls. If the following continuation provider round fails before
visible output, that continuation request may retry with the already-recorded tool result; the tool
batch itself is not executed again. Tests cover this with an `ExecuteProcess`-classified tool and
prove exactly one invocation. Provider failures after visible output are terminal, so retries cannot
replay tool-call output either.

Applications that need idempotent retry across process loss or external transport replay must use
their own tool idempotency keys; Bcode does not falsely infer external side-effect safety.
