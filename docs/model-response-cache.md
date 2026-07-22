# Model response cache contract

Bcode caches only completed non-streaming SDK responses. Streaming always bypasses the response
cache because replay cannot reproduce trustworthy event timing, cancellation, provider/tool
lifecycle, or partial consumption.

## Identity and privacy

`ModelResponseCacheKey::from_request` hashes a versioned canonical JSON identity with SHA-256. The
identity includes provider plugin, model, provider context/config/auth selection, complete messages
and prompt, tools and tool policy, structured-output schema, parameters, metadata, timeout, round
bounds, and retry/fallback/routing identity. The public key contains only a schema version and hex
digest; it does not retain prompt, credential, or provider-extension values.

Custom `ProviderRoundPlanner` implementations disable caching until the application supplies a
stable versioned `cache_routing_identity`. Requests with custom stop predicates also bypass caching
because opaque closure behavior cannot be keyed. `ModelResponseCachePrivacy::NoStore` bypasses both
lookup and storage. Applications decide whether `Private` or explicitly `Shared` entries may enter
their storage and must enforce tenant/user isolation outside the digest when appropriate.

Requests advertising tools bypass caching by default: a hit would suppress tool execution and could
skip side effects. `allow_tool_responses`/`with_tool_responses(true)` is an explicit opt-in for
applications that have proven replay safety and version tool implementations in request metadata.

## Stored response and usage

A cache entry is a complete `GenerateTextResponse`, including canonical text, ordered model/tool
steps, structured/tool results, provider metadata, stop reason, latency, and provider-reported
usage. On a hit those fields are historical evidence from the original provider execution; cached
usage is not new billable usage. `ModelResponseCacheStatus::{Stored,Hit,Bypassed}` makes provenance
explicit.

Request middleware runs in registration order before key derivation and lookup. Cache storage gets
the successful provider/tool response before response middleware. Response middleware runs in
reverse order and model after-hooks run for both hits and misses. Thus application response
transforms and observations occur per SDK call and are never accidentally baked into storage.

## Expiration, invalidation, and capacity

Adapters own expiration, invalidation, capacity, serialization, encryption, and storage errors.
`ModelResponseCache::invalidate` removes one complete post-middleware request identity;
application adapters may expose broader invalidation APIs. The bundled `InMemoryModelResponseCache`
requires TTL and non-zero capacity, evicts oldest inserted entries when full, supports exact and
full invalidation, and never persists data.

## Stampede control and failure

A miss adapter may reserve the key. Followers block on Tokio's blocking pool, not an async executor
worker. `put` commits and wakes followers; `abort` releases a failed leader. The bundled in-memory
adapter also expires single-flight leases (30 seconds by default, configurable), so an abandoned or
dropped leader cannot strand followers forever. Provider/tool failures are never cached. Cache
lookup/storage failures are typed terminal SDK errors rather than silent corruption.

The compatibility `ModelResponseCache` interface remains application-owned. Implementations that do
not reserve misses can keep `abort` as its no-op default, but distributed adapters must document and
implement atomic miss reservation if they claim stampede control.
