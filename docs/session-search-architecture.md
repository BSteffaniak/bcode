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
or other persistence implementation details.

Normal investigation reads are non-mutating. They do not migrate, repair, rebuild, reindex, or
silently reinterpret unsupported canonical state. Missing, damaged, incompatible, and stale state is
reported explicitly according to the session persistence architecture.

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

Session investigation remains available when no search provider exists. Search providers can be
absent at compile time, disabled at runtime, or excluded from a content route. A disabled provider:

* opens no provider index or corpus;
* starts no indexing or scan workers;
* receives no session content;
* allocates no backend writer/cache resources;
* cannot affect session creation, append, listing, attach, history, export, or structured inspection.

Disabling a provider does not delete its data. Purge is a separate explicit maintenance operation.
Tantivy is one possible provider implementation and must remain confined to its provider crate and
feature path. Its types and query language do not enter shared contracts.

## Search-record projection

Providers receive an allowlisted searchable projection, not raw `SessionEvent` values or serialized
event payloads. Projection policy explicitly chooses finalized content such as titles, user and
assistant messages, optional reasoning, commands, bounded tool output, errors, and compact metadata.
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
queries across configured content routes. Routes may identify primary, fallback, parallel, or
disjoint-coverage providers. Queries are not broadcast blindly when providers overlap.

Eligible providers may execute concurrently with end-to-end cancellation, bounded concurrency,
per-provider deadlines, and an overall deadline. Fast indexed results are not held indefinitely by
cold or deep scans. Provider scores are opaque and are not treated as directly comparable across
backends. Results are grouped or combined using backend-neutral rank information, then deduplicated
by canonical locator.

Every response reports what was searched and what was not, including provider completion, timeout,
failure, unsupported features, stale checkpoints, content exclusions, truncation, quotas, and
covered ranges. Query-execution completeness and corpus-coverage completeness are distinct.

## Ingestion and maintenance

Canonical append succeeds or fails independently of search. Only after a successful append may a
bounded coordinator mark a session dirty. Repeated notifications are coalesced, and providers catch
up from bounded canonical pages using idempotent batches.

Provider-owned checkpoints advance only after derived data is durably published. Checkpoints bind to
trustworthy canonical generation/fingerprint data and projection, normalization, and content-policy
versions. Mismatch produces stale or rebuild-required state; it never causes silent reuse or merge.

Historical backfill, full rebuild, policy-change reindexing, and purge are explicit cancellable
maintenance operations. They do not run from daemon startup, catalog listing, attach, ordinary
history, rendering, model-context construction, or ordinary search. Search failure, quota
exhaustion, corruption, or provider absence never delays or rolls back canonical session writes.

## Large-output search

Large normalized shell/tool output may use a provider specialized for compressed chunks or bounded
scanning rather than the transcript index. Provider-owned chunks must be independently bounded,
checksummed, confined, and cancellable to scan. Ordinary search can exclude cold output; explicit
deep search reports its longer-running providers and partial coverage.

Whether the first implementation uses Tantivy, an in-process compressed scanner, `rg` behind a
confined adapter, Turso/SQLite FTS, or another backend is an implementation decision. Shared session
search semantics do not depend on that choice.

## Privacy and resource controls

Reasoning, command text, tool arguments, shell output, other tool output, permissions, diagnostics,
traces, and artifacts have independent policy controls. Sensitive/high-volume categories use
conservative defaults and explicit limits.

Providers enforce bounded records, batches, responses, snippets, writer memory, concurrency,
invocation/session content, and total derived storage. Reaching a quota preserves valid derived data,
stops or narrows advancement, and reports incomplete coverage. It does not silently claim
completeness or mutate canonical history.

Provider paths are canonicalized and confined to authorized derived-state roots. Public errors,
diagnostics, logs, and metrics are bounded and secret-safe. Remote providers require separately
approved configuration and authorization before receiving session content.

## Frontend and skill boundary

Public search and investigation contracts remain portable. Renderers adapt shared semantic results
without owning provider workflows or canonical session state. The TUI owns terminal layout and
interaction only.

The `bcode-session-history` skill uses supported Bcode commands and reports session IDs, query scope,
content/provider coverage, truncation, freshness, and failures. Until indexed cross-session search is
implemented, the skill may explicitly state that broad full-text search is unavailable; it does not
fall back to direct database access.
