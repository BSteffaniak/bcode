# Session Persistence Architecture

Bcode keeps session `*.events` files as the canonical append-only history, but daemon startup must not replay every persisted transcript. The scalable architecture is a two-layer store:

* `sessions/<session-id>.events` is the durable source of truth.
* `sessions/index/<session-id>.index.json` is a rebuildable sidecar cache for catalog/startup metadata.

The index is never authoritative. If it is missing, stale, corrupt, or version-incompatible, Bcode rebuilds it from the event log.

## Current implementation direction

Startup now loads session catalog state from fresh sidecar indexes when possible. A valid index contains enough data for session listing and safe appends without decoding the full event payload stream:

* session id and index version
* event file fingerprint: length and mtime
* session summary/title
* next sequence number
* decoded event count
* whether a user message exists, for auto-title behavior
* latest model/provider selection
* latest agent selection
* latest compaction sequence
* metered token total
* corruption/degradation records from index rebuild

Full event history is loaded lazily when a caller asks for history or attaches to a session. Newly created or already-loaded sessions keep an in-memory history cache; catalog-only sessions do not.

## Correctness rules

* The event log is canonical. Deleting an index must never lose session data.
* Index writes are atomic temp-file + rename writes.
* Event appends happen before index refresh; if the index refresh fails, the event remains durable and the next startup can rebuild the cache.
* `SessionEventKind` variant order is persisted by `bmux_codec` and must not be reordered. New variants are append-only and require a schema-version bump.
* Future frame formats should add explicit magic/version/checksum metadata instead of relying only on enum order.

## Corruption handling

The reader classifies event-file problems as truncated frame length, truncated payload, oversized frame, or decode failure. Rebuilds record these issues in the sidecar index so daemon startup does not repeatedly spam per-frame decode logs.

A future repair command should quarantine or truncate only clearly invalid tails after creating a backup. Middle-frame decode failures should mark the session degraded because the frame boundary may still be structurally valid even if the payload schema is incompatible.

## API direction

The legacy full-history APIs remain for compatibility, but new code should prefer bounded and projected access:

* `SessionManager::session_history_page`
* IPC `SessionHistoryPage`
* client `session_history_page`
* lightweight selection projections such as current agent and current model

The TUI can migrate to recent-page attach plus scrollback paging. Model request construction still needs a model-visible projection of history, but it should use projection helpers and compaction boundaries rather than requiring all daemon startup state to be hydrated.

## Maintenance commands

CLI maintenance commands provide operator ergonomics:

* `bcode session doctor` reports index freshness, event counts, offsets, and issue counts.
* `bcode session reindex` rebuilds all sidecar indexes from canonical event logs.

## Migration stages

1. Add sidecar indexes and lazy catalog startup.
2. Add bounded history APIs over IPC/client/session.
3. Move server hot paths that only need current model/agent to index-backed projections.
4. Migrate TUI attach to recent-page loading and scrollback paging.
5. Introduce a versioned/checksummed v2 event frame while keeping the legacy reader.
