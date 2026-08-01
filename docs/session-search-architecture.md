# Session Investigation and Federated Search Architecture

## Purpose

Bcode provides session investigation through application-owned read boundaries rather than requiring
skills, frontends, plugins, or external processes to understand or open canonical session storage.
Search extends that investigation boundary with optional derived providers. It never changes the
authority of canonical session history.

## Canonical investigation boundary

The per-session canonical `events` table remains authoritative. The session domain owns decoding,
sequence semantics, compatibility reporting, and bounded canonical reads. Current read classes are:

* bounded history pages for ordinary navigation;
* bounded around-sequence windows for hydrating stable event locators;
* bounded typed inspection pages for failed tools, permissions, selection changes, runtime work,
  compactions, and terminal outcomes;
* explicit complete history for export and debugging, with its full-history cost clearly separated
  from normal reads.

The daemon exposes these operations through portable IPC/client contracts. CLI commands and skills
consume those contracts. They do not open `catalog.db`, `session.db`, WAL files, projection tables,
or other persistence implementation details. The CLI exposes backend-neutral structured search
filters for canonical session IDs, working directory, timestamp bounds, tool name/status,
provider/model/agent identity, import source, content category, and stable semantic match fields. The
CLI also exposes terms, phrase, prefix, regex, and fuzzy match modes so unsupported provider features
remain explicit during planning rather than being approximated. Ordinary search remains the
default; `--deep` explicitly selects the plan policy that may invoke cold scan providers. CLI JSON
flattens each hit into stable automation fields (session/event locator, content and match field,
canonical timestamp when hydrated, bounded preview/truncation, provider rank/score, and hydration
outcome) alongside explicit query/corpus completeness, provider reports, failures, execution class,
and a normalized terminal outcome.

Normal investigation reads are non-mutating. They do not migrate, repair, rebuild, reindex, or
silently reinterpret unsupported canonical state. Missing, damaged, incompatible, and stale state is
reported explicitly according to the session persistence architecture.

## Public read semantics

Bounded history and inspection contracts use canonical event sequence ordering. Forward reads return
ascending sequences; backward reads select from newest to oldest but return the selected page in
chronological ascending order for presentation. Cursors are inclusive canonical sequence locators.
`next_cursor`, when present, identifies the first canonical candidate not returned by the current
page. Reissuing the same immutable canonical query is safe and deterministic. Session events are
append-only, so a retry may observe a newer tail but cannot change the identity or meaning of an
already committed sequence.

Read requests are side-effect free and therefore safe to retry or deliver more than once. Responses
do not acknowledge durable progress and cursors do not imply reconnect-safe retention beyond
canonical session history. A request has one terminal response: success or a normalized error.
Late/stale process-local updates cannot reopen that response. Missing sessions, incompatible active
ownership, future/unknown formats, stale projections, exceeded bounds, and repair-required damage
have distinct public error classifications.

A normal history page or around window returns at most
`MAX_SESSION_HISTORY_READ_EVENTS` events. Structured inspection returns at most
`MAX_SESSION_INSPECTION_EVENTS` matches and decodes at most
`MAX_SESSION_HISTORY_READ_EVENTS` prefiltered canonical candidates per page. Persisted durable event
payloads are bounded at append boundaries by event-kind-specific rules; generic durable
contributions are capped at 64 KiB. IPC framing and client request timeouts provide additional
transport bounds. Complete selected-session export is explicitly exempt from normal-read event-count
bounds and must never be reused as ordinary search hydration.

## Backend-neutral contract

The shared Rust contract is implemented in `packages/session-search` and exposed to plugins as:

```text
bcode.session_search/v1
```

Query operations are `search`, `capabilities`, and `status`. Maintenance operations are
`apply_batch`, `remove_session`, and `purge`. The contract defines bounded boolean text queries,
explicit terms/phrase/prefix/regex/fuzzy modes, structured session filters, deterministic portable
sort/cursor envelopes, canonical event locators, provider-local ranks/scores, capability negotiation,
coverage/freshness/status, normalized errors, and idempotent ingestion batches.

Regex and fuzzy semantics are requested explicitly and may only be routed to providers advertising
the corresponding capability. Unsupported behavior is returned as `unsupported_query`; it is never
silently approximated. Provider scores are opaque and only comparable within one provider response.
Cursors carry provider identity and query fingerprint but keep backend pagination state opaque.

The contract crate depends on portable session models and serialization only. It contains no
Tantivy, SQL, daemon, renderer, or generic plugin-host implementation types. The server's initial
session-search adapter is a real caller: it discovers service registrations, requests typed
capabilities/status, validates provider identity and response bounds, and invokes one exact provider.
Federated routing remains a subsequent application-layer phase.

## Search ownership

Session search is a session-domain capability implemented through versioned typed plugin services.
The shared contract is backend-neutral but intentionally not domain-neutral: it models session text,
content categories, structured filters, canonical event locators, provider capabilities, freshness,
and coverage.

The generic plugin host owns only discovery, loading, lifecycle, isolation, typed invocation,
cancellation plumbing, concurrency, and payload bounds. It does not own search semantics, content
routing, ranking, indexing policy, or canonical session access.

Search providers own their implementation details, including:

* backend schemas, tokenization, ranking, and query conversion;
* derived index/corpus files and confined storage paths;
* checkpoints, commits, merge/compaction policy, caches, and quotas;
* corruption detection, purge, and rebuild behavior;
* accurate capability, freshness, exclusion, truncation, and degraded-state reporting.

A provider never opens canonical Bcode databases and never writes canonical session state. It
receives bounded versioned search records projected by Bcode from finalized semantic events.

## Complete disablement

Session investigation remains available when no search provider exists. Global
`[session_search].enabled` is independent from generic plugin selection and defaults to `true`. When
false, the daemon starts no search ingestion worker, schedules no dirty work, exposes no active search
provider inventory, invokes no query or maintenance operation, and sends no session content to a
provider; canonical list/history/around/inspection/export APIs remain available. Search providers can
also be absent at compile time, disabled at runtime, or excluded from a content route. A disabled
provider:

* opens no provider index or corpus;
* starts no indexing or scan workers;
* receives no session content;
* allocates no backend writer/cache resources;
* cannot affect session creation, append, listing, attach, history, export, or structured inspection.

The operational acceptance matrix spans layers: the architecture guard verifies compile-time absence;
configuration tests verify global enablement parsing/defaults; contract planning tests verify route
disablement and unsupported future capabilities; server tests verify no inventory/query/maintenance/
ingestion under global disablement and sanitize malformed providers; provider tests verify retained data
after disable, explicit purge, quota behavior, confined paths, incompatible versions, and sensitive
content defaults.

Disabling a provider does not delete its data. Re-enabling the same configured provider root reuses
retained compatible derived state. Purge is a separate explicit maintenance operation requiring the
exact provider identity and provider-defined confirmation token; it revalidates root confinement at
deletion time and removes only the provider directory while preserving its parent. Tantivy is one
possible provider implementation and must remain confined to its provider crate and feature path.
Its types and query language do not enter shared contracts.

## Search-record projection

Providers receive an allowlisted searchable projection, not raw `SessionEvent` values or serialized
event payloads. Projection policy explicitly chooses finalized content such as titles, user and
assistant messages, optional reasoning, commands, bounded tool output, errors, and compact metadata.
For ingestion, the selected provider's advertised content kinds are the host allowlist: Bcode derives
each independent sensitive-category gate from those capabilities and filters projected records again
before invoking the provider. A provider therefore cannot receive a content category it did not
advertise, while provider configuration may conservatively omit categories from its capabilities.
New event fields do not become searchable automatically.

Transport-level deltas, replaceable progress, raw event JSON, binary artifacts, and trace blobs are
not indexed as independent session-search records. Terminal-like output is normalized through a
versioned bounded pipeline before crossing the provider boundary. Search records retain canonical
session/event locators and enough coverage metadata to report source bytes, indexed bytes,
truncation, and policy exclusions.

Search hits are derived candidates. Canonical surrounding context is hydrated through bounded
session reads using stable locators. Missing or stale locators are surfaced rather than guessed.

## Federation and query planning

A session-search coordinator at the application boundary discovers provider capabilities and plans
queries across configured content routes. The generic plugin runtime retains its resolved selection
inventory so explicitly disabled or configured-unavailable search providers can be reported before
loading; non-loaded providers are identified only by the `session-search` plugin naming convention or
a plugin-owned `session_search_provider = true` configuration marker. Routes may identify primary,
fallback, parallel, or disjoint-coverage providers. Queries are not broadcast blindly when providers
overlap.

Eligible providers may execute concurrently with bounded concurrency, independently configured
per-provider deadlines capped by an overall deadline, and end-to-end cancellation. Timed provider
calls cancel the generic runtime invocation token propagated through the plugin ABI before returning
their terminal timeout; dropping a future alone is not treated as cancellation. Planning has an explicit portable execution class: ordinary search rejects every scan provider with
an explicit deep-required outcome and applies a configured canonical sequence-lag threshold, while
only an explicit deep request (the CLI `--deep` flag) may select scan providers and relax freshness.
Fast indexed results are not held indefinitely by cold or deep scans. Provider scores are
opaque and are not treated as directly comparable across backends. Results are grouped or combined
using backend-neutral rank information, then deduplicated by canonical locator.

Every v1 response is one bounded terminal aggregate; partial provider response streaming is deferred.
Every provider contribution or failure identifies its terminal stage, elapsed time, requested/searched
or hydration-affected content, completion, timeout/failure classification, stale checkpoints, content
exclusions, truncation, quotas, and covered ranges. Failed canonical hydration adds a hydration-stage stale-provider outcome and makes query/coverage
completeness false rather than leaving a successful provider report unqualified. Future persisted
event schemas/kinds, projection schemas, writer epochs, and ownership classify as incompatible;
inconsistent migration history, invalid canonical sequences/rows/compaction markers, stale
projections, and unavailable/corrupt canonical DB state classify as repair-required. Failed
hydration never substitutes another event or returns guessed canonical content. Query-execution
completeness and corpus-coverage completeness are distinct.

## Ingestion and maintenance

Canonical append succeeds or fails independently of search. Only the existing post-commit mutation
notification may mark a session dirty. The server coalesces loaded-provider notifications in a
bounded 1,024-session set; duplicates collapse and overflow/subscriber lag records an explicit
rescan-required bit rather than allocating unbounded work. A detached worker discovers providers,
reads at most each provider's advertised record limit from forward canonical history, applies the
allowlisted finalized-event projection, validates the portable batch, and invokes `apply_batch`.
The worker also enforces advertised text limits and can advance through an event-only page using an
explicit indexed-through sequence without fabricating searchable records. A scheduling slice drains at
most 16 pages per provider, requeues remaining work, and retries failed sessions after 100 ms, keeping
catch-up bounded and detached from canonical interaction. Bounded operational metrics record dirty-batch
size, page events, batch records/text bytes, per-session duration, completion/failure, retry, and slice
requeue without session IDs or provider-controlled labels. Retry classification treats stale generation,
checkpoint conflict, quota exhaustion, disabled content, invalid requests, and typed response
incompatibility as terminal for the dirty item; only transient transport/service failures retry.
Provider-side expected sequence/text checks remain the atomic stale-write guard. The host additionally
validates that every apply-batch acknowledgment has the requested batch identity, requested terminal
sequence, and outcome-consistent record count; conflicting duplicates are terminal rather than being
mistaken for successful progress. Canonical deletion commits a durable provider-owned generation
tombstone with the same Tantivy commit marker as document removal, so in-flight or post-restart stale
batches for the deleted generation cannot recreate derived records. Provider coverage
includes retained indexed text bytes alongside the sequence checkpoint so the next bounded batch can
satisfy cumulative limits.
That checkpoint is provider-owned durable state, not a reconnect-safe transport cursor; the transport
defines no retention, acknowledgment replay, or conflict protocol beyond each idempotent invocation.

Provider-owned checkpoints advance only after derived data is durably published. Checkpoints bind to
a versioned SHA-256 of stable canonical session identity, creation identity, and import/fork lineage
facts/cutoffs, plus projection, normalization, content-policy versions, quota, and the provider's
exact configured content-kind allowlist. Mutable title, activity, working-directory, and client-count
fields do not change generation. A changed content allowlist or quota produces explicit degraded,
rebuild-required status rather than reusing prior coverage; canonical generation mismatch likewise
produces terminal stale or rebuild-required state before paging and never causes silent reuse or
merge.

Historical backfill, full rebuild, policy-change reindexing, and purge are explicit cancellable
maintenance operations. The public client/IPC/CLI boundary exposes provider-scoped purge, empty
rebuild, and daemon-owned bounded backfill. `session search-backfill` selects either explicitly named
canonical sessions or at most 256 catalog sessions filtered by summary update timestamps and an
explicit stable continuation cursor, applies a
bounded wall-clock deadline, and processes no more than 1,024 provider-sized canonical pages per
session. It resumes from provider-owned durable sequence/text checkpoints, reports per-session
through/tail progress plus incomplete/failure/deadline and selection-truncation state, and uses
cancellation-propagating deadlines for provider ingestion calls. The operation never hands canonical
storage paths or event envelopes to providers. Reissuing the request is restart-safe because each
batch has a deterministic canonical sequence identity and provider-side expected checkpoint checks;
this does not imply transport-level durable resume or replay.

Canonical deletion remains authoritative and occurs first; after success the
server best-effort invokes each loaded provider's typed `remove_session` with the expected generation,
without rolling canonical deletion back on provider failure. The Tantivy provider's rebuild operation
is deliberately provider-local: an exact provider-specific confirmation detaches live reader/writer
state, removes only the canonicalized confined derived root, and creates an empty index and checkpoint
at current versions. It does not read
or replay canonical history; daemon-owned maintenance must refill it through bounded idempotent
batches. Full historical work runs only after the explicit backfill command; daemon startup,
catalog listing, attach, ordinary history, rendering, model-context construction, and ordinary
search never schedule it. Search failure, quota
exhaustion, corruption, or provider absence never delays or rolls back canonical session writes.

## Large-output search

Large shell/tool output is an explicit deep-search category. The shared planner rejects requests that
name `shell_output` or `tool_output` under ordinary execution even if an indexed provider advertises
those categories; callers must opt into deep policy. This prevents an accidental policy change or
provider capability expansion from making ordinary transcript search scan or expose large output.

The first implementation choice remains evidence-gated. Supported canonical export attempts against
several representative local sessions on 2026-08-01 produced only explicit degraded evidence: the
active session was unavailable because its canonical WAL owner held the lock, while other sampled
sessions reported an unknown migration and required repair. No direct database fallback was used, so
there is not yet a trustworthy representative output-size/query-frequency corpus from which to select
a backend.

Large normalized shell/tool output may use a provider specialized for compressed chunks or bounded
scanning rather than the transcript index. Provider-owned chunks must be independently bounded,
checksummed, confined, and cancellable to scan. Ordinary search can exclude cold output; explicit
deep search reports its longer-running providers and partial coverage.

Whether the first implementation uses Tantivy, an in-process compressed scanner, `rg` behind a
confined adapter, Turso/SQLite FTS, or another backend is an implementation decision. Shared session
search semantics do not depend on that choice.

## Privacy and resource controls

Reasoning, command text, tool arguments, shell output, other tool output, permissions, diagnostics,
traces, and artifacts have independent projection-policy gates. The default projection includes
shell command text but excludes reasoning, shell output, tool arguments, successful generic tool
output, permissions, runtime diagnostics, traces, and artifacts. The Tantivy provider separately
requires sensitive categories to be explicitly allowlisted before accepting projected records.
Commands, arguments, reasoning, and tool output may contain credentials or private data; enabling a
category copies its bounded normalized text into disposable provider-owned derived storage until
explicit purge. Trace projection includes only allowlisted semantic
summaries and identifiers; trace blobs, blob paths, opaque metadata, and request/output payloads are
excluded. Artifact projection includes only identity, producer, schema, title, and reference count;
opaque metadata, storage URIs, and artifact bytes are excluded.

Projection records report original source bytes, retained/inspected source ranges, normalized bytes,
indexed bytes, truncation, and normalization/policy versions. Split shell stdout/stderr records retain
their own canonical byte totals rather than inheriting an invocation-wide total; aggregate invocation
bytes remain explicit metadata.

Provider ingestion has explicit resource bounds at every current layer:

* one normalized record is capped at 64 KiB;
* one portable batch is capped at 256 records, 4 MiB normalized text, and 8 MiB serialized payload;
* one canonical session is capped at 256 MiB cumulative normalized text per provider;
* one federated query is capped at eight concurrent providers, 200 hits per provider, 4 KiB previews,
  and explicit per-provider/overall deadlines;
* the Tantivy provider defaults to a 32 MiB writer cache (15 MiB minimum) and a 2 GiB derived-state
  quota; configured quota and content-policy changes require explicit rebuild;
* the asynchronous server drains at most 16 bounded pages per provider/session scheduling slice and
  retains at most 1,024 coalesced dirty sessions.

The current transcript provider has no automatic eviction: reaching any bound preserves committed
derived state, stops or narrows advancement, and reports incomplete/quota state. A future large-output
provider must define its own total corpus, chunk, cache, concurrency, and retention bounds before it
can be enabled; these transcript limits do not pre-decide that backend.

Provider ingestion has three independent content bounds before a batch may cross the plugin boundary:
individual normalized record text, aggregate normalized text and serialized payload for one invocation,
and cumulative normalized text for one canonical session. The session aggregate is carried as an
expected prior byte count that providers compare with atomically retained accounting; it is quota
coordination, not a durable-resume cursor. Providers additionally enforce writer-memory and total
derived-storage quotas. The Tantivy provider enables LZ4 postings and Zstd columnar compression while
keeping Tantivy default features, stopwords, and stemming disabled; a deterministic release benchmark
records the resulting latency, commit, reopen, and storage-amplification behavior in
`docs/session-search-baseline.md`. Reaching a quota preserves valid derived data, stops or narrows
advancement, and reports incomplete coverage. It does not silently claim completeness or mutate
canonical history.

Per-provider enablement is represented by loaded provider inventory plus explicit backend-neutral
content routes. Routes select provider IDs with primary, fallback, parallel, or disjoint semantics;
providers omitted from an applicable route are not invoked. Shared query AST/filter semantics remain
backend-neutral even though deployment configuration names concrete provider plugins.

Provider status is the operational source for active provider identity, execution kind, advertised
content policy/features, shared projection versions, index/quota/document/pending counts, degraded
reason, and per-session generation/tail/checkpoint/content/byte/completeness/exclusion state.
Explain-plan carries the explicit backend-neutral routes plus selected and excluded providers. CLI
text and JSON expose those contracts without reading provider files.

Remote providers fail closed independently of ordinary/deep policy. Discovery, loading, routing, or
an explicit deep request is not authorization: the portable plan policy must separately set
`allow_remote = true` before a provider advertising `Remote` execution can be selected to receive
projected session content. Frontends do not currently expose that opt-in by default.

Provider paths are canonicalized and confined to authorized, explicitly configured derived-state
roots. A provider root must be an absolute non-root directory rather than a file, traversal,
filesystem root, or symbolic-link provider boundary. Existing roots are canonicalized; new roots use
a canonical parent plus one final provider-owned component. Tantivy stores only its `index/` and
versioned checkpoint files there. Destructive operations revalidate the boundary and remove only that
provider root, never canonical session directories. Provider errors are passed through the shared
credential sanitizer before character-bounded truncation. The server rejects previews and diagnostic
messages above 4 KiB and bounds opaque scores, record IDs, and cursors; Tantivy creates UTF-8-safe
bounded previews. Session-search metrics use fixed host-owned names without query text, session IDs,
paths, or provider-controlled labels. Focused tests cover credential-shaped diagnostics, oversized
responses, and UTF-8-safe truncation. Remote providers require separately approved configuration
and the explicit portable `allow_remote` authorization described above before receiving session
content.

Operational setup, supported CLI workflows, status interpretation, maintenance, recovery, and
measured limits are documented in [`session-search-operations.md`](session-search-operations.md).

## Frontend and skill boundary

Public search and investigation contracts remain portable. Renderers adapt shared semantic results
without owning provider workflows or canonical session state. The TUI owns terminal layout and
interaction only.

The intended `bcode-session-history` skill contract is to use supported Bcode commands and report
session IDs, query scope, content/provider coverage, truncation, freshness, and failures. A native
session workflow must not fall back to direct database access when optional providers are absent or
coverage is incomplete. The installed user-config skill still requires a separately authorized
migration before it satisfies this contract; architecture documentation does not treat that external
file as already migrated. A 2026-08-01 write attempt was rejected by the active filesystem permission
boundary before changing the skill.
