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

A miss adapter may reserve the key. Lookup and storage run through Switchy's blocking-task
boundary (Tokio's blocking pool in the native build), not directly on the async executor worker.
After a successful lookup returns a miss, an SDK-owned guard carries cleanup responsibility
through generation and storage. Successful `put` disarms the guard; failure or future drop calls
`abort` exactly once. An abandoned blocking lookup result also releases its late miss. `abort`
can run synchronously during drop and must perform bounded cleanup without waiting for other
requests or panicking. A lookup that fails before returning a miss owns its own partial cleanup.

Already-observed request cancellation prevents lookup dispatch. Cancellation during lookup returns
a typed cancellation result without waiting for the adapter. It does not interrupt synchronous
adapter execution: the task may finish later, and its result owns any miss cleanup. These native
regressions do not certify deterministic scheduling or complete runtime isolation. Storage already
dispatched to a blocking task can likewise finish after its caller drops; drop does not roll back
storage.

The bundled in-memory adapter also expires single-flight leases (30 seconds by default,
configurable). Provider/tool failures are never cached. Cache lookup/storage failures are typed
terminal SDK errors rather than silent corruption.

### Known borrowed-provider future-drop limitation

Cache reservation cleanup is separate from provider-turn cleanup. Dropping a buffered SDK
`generate_text_with_provider_and_cancellation` future while its borrowed provider is active does
not currently invoke that provider's cancel/finish lifecycle. The SDK smoke regression observes
zero finish calls where one is required on both native and simulator backends. Dropping an owned
in-process provider adapter has separate cleanup and does not establish safety for this borrowed
call path. The uncached `generate_text_with_provider` regression also fails: its runtime scope
is released, but no provider finish is observed within the cleanup watchdog. Disabling the response
cache therefore does not avoid this lifecycle defect.

Until this is fixed, cancel through the supplied cancellation token and continue awaiting the
buffered call's terminal result rather than dropping it to request cancellation. Explicit token
cancellation and deadline cases pass the smoke scenario before its future-drop case fails. This
is not a guarantee for arbitrary provider implementations or their independently spawned tasks.

### Known reservation-fencing limitation

The current `get`/`put`/`abort` contract identifies operations by request key, not by a unique miss
reservation. Invalidation or lease replacement therefore does not fence an older leader: a late
`put` can overwrite a newer response, and a late `abort` can release a newer reservation. The SDK
cleanup guard prevents abandoned ownership from leaking but does not solve this identity defect.
Do not treat invalidation as a barrier against in-flight completions or claim ownership-fenced
stampede control. Reservation identity must be carried from lookup through completion and abort
before that guarantee can be made.

The compatibility `ModelResponseCache` interface remains application-owned. Implementations that do
not reserve misses can keep `abort` as its no-op default. Distributed adapters must define their
atomic reservation and stale-owner behavior explicitly; the key-only interface alone supplies no
such guarantee.
