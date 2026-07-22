# Rate-limit handling

Bcode separates provider-reported limits from application-owned admission control.

## Provider limits

Provider failures use `ProviderErrorCategory::RateLimit`. `ProviderError.retry` carries typed retry
information, including absolute reset time and provider-supplied delay when available. Provider auth
usage uses typed windows with limit, remaining usage, and reset timestamps; providers that support
banked reset credits expose them through the versioned auth operations. Missing provider fields
remain `None` rather than being estimated or fabricated.

Retry metadata is diagnostic and does not authorize an automatic retry. Retry/fallback policy owns
that decision and must still honor whole-turn cancellation and safety constraints.

## Application-owned limiters

`ApplicationRateLimiter` receives the complete transport-independent `AgentTurnRequest`, including
provider/model selection, metadata, messages, tools, parameters, and timeout. Implementations own:

* request keys and tenant/application identities;
* local or distributed storage;
* atomic counters and windows;
* synchronization, persistence, and availability policy;
* whether and when capacity is reserved or refunded.

`RateLimitMiddleware` invokes the limiter at the normal pre-provider middleware boundary for both
streaming and non-streaming requests. `Allow` continues normally. `Deny` returns typed
`BcodeError::RateLimited` with limiter ID, reason, and optional absolute retry time before network
work. Limiter/storage errors return typed `BcodeError::RateLimiter` and fail closed. Bcode does not
couple the lean core to a database, cache server, or distributed-lock implementation.

The limiter runs once for the complete logical SDK request, not once per provider retry or tool-loop
continuation. Applications needing per-provider-attempt accounting should implement that policy in
a provider round planner rather than accidentally double-charging middleware admission.
