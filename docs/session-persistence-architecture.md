# Session Persistence Architecture

Bcode keeps session `*.events` files as the canonical append-only history, but normal daemon startup, catalog listing, session opening, and paged history reads must not replay every persisted transcript. The scalable architecture uses canonical logs plus rebuildable sidecars:

* `sessions/<session-id>.events` is the durable source of truth.
* `sessions/index/<session-id>.index.json` is the primary metadata sidecar for catalog/startup metadata.
* `sessions/index/<session-id>.entries.jsonl` is the primary entry sidecar for sequence-to-offset lookup.
* Derived sidecars under `sessions/index/<session-id>/` store projection-specific data such as transcript spans and input history.

The event log is authoritative, but normal runtime paths must not automatically rebuild indexes from it. Full event-log replay belongs to explicit maintenance operations such as doctor-with-fix, reindex, repair, or migrations.

## Current implementation direction

Startup loads session catalog state from fresh sidecar indexes when possible. A valid metadata index contains enough data for session listing and safe appends without decoding the full event payload stream:

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

Paged history reads use the per-event JSONL entry index (`sequence`, byte offset, frame length, event kind, schema version) and seek directly to requested frames. Newly created sessions keep an in-memory history cache until restart; catalog-only restored sessions do not hydrate full histories for listing, status, paged history, or recent attach.

When the metadata index is stale but the entry index is trustworthy, normal paths may catch metadata up incrementally from the indexed tail. When sidecars are missing, corrupt, internally inconsistent, or otherwise untrustworthy, normal open/attach/history paths should return an explicit repair-required error instead of replaying the whole log.

## Catalog discovery rules

The catalog is best-effort and non-mutating for damaged sessions:

* A bad session file or bad sidecar must not fail the whole native session catalog.
* Catalog discovery may do bounded first-event discovery from the canonical event log to recover display metadata.
* If an event log has an unindexed tail after that first frame, catalog discovery must not write partial metadata or entry indexes.
* Synthesized catalog entries with unknown/unindexed tails must be treated as degraded or read-only until explicit repair/reindex validates the log.
* Bad entry indexes, such as a first entry that does not point at offset `0` or does not identify `session_created`, must not be trusted for catalog synthesis.
* Catalog status should surface degraded/native repair-needed state without triggering repair.

This preserves the important distinction between discovery and repair: catalog listing can show what is safely discoverable, but only explicit maintenance commands may scan and rebuild from canonical logs.

## Correctness rules

* The event log is canonical. Deleting an index must never lose session data.
* Index writes are atomic temp-file + rename writes.
* Normal catalog/open/attach/history/model-context paths must not call full event-log replay or repair rebuild functions.
* Missing/corrupt sidecars on normal paths should become degraded or repair-required state, not implicit rebuilds.
* Derived indexes are projection caches. Missing/invalid derived state should degrade that projection or require repair; stale-but-valid derived state may catch up incrementally from its checkpoint.
* Repair should quarantine or truncate only clearly invalid tails after creating a backup. Middle-frame decode failures should mark the session degraded because the frame boundary may still be structurally valid even if the payload schema is incompatible.

## API direction

The legacy full-history APIs remain for compatibility, but new code should prefer bounded and projected access:

* `SessionManager::session_history_page`
* IPC `SessionHistoryPage`
* client `session_history_page`
* lightweight selection projections such as current agent and current model

The TUI uses a recent-page attach path for initial load and requests/prepends older pages when the user scrolls near the top of the loaded transcript. Model request construction uses a session projection that starts at the latest compaction boundary when available instead of hydrating the entire raw log.

## Maintenance commands

CLI maintenance commands provide explicit repair ergonomics. These commands may scan canonical event logs and rebuild sidecars:

* `bcode session doctor [session-id]` reports index freshness, event counts, offsets, and issue counts.
* `bcode session reindex [session-id]` rebuilds sidecar indexes from canonical event logs.
* `bcode session repair <session-id>` backs up and truncates only unreadable tails, then rebuilds indexes.

Do not invoke these repair paths implicitly from catalog listing, session picker display, normal attach, or paged history reads.

## Architecture guardrails

Run the session architecture guard before finishing session persistence changes:

```sh
scripts/check-session-architecture.sh
```

This includes the normal-path full-scan guard and checks for session actor/store layering violations.

## Migration stages

1. Add sidecar indexes and lazy catalog startup.
2. Add bounded history APIs over IPC/client/session.
3. Move server hot paths that only need current model/agent to index-backed projections.
4. Migrate initial TUI attach to recent-page loading.
5. Introduce a versioned/checksummed v2 event frame while keeping the legacy reader.
6. Keep normal paths bounded and move full replay/rebuild behavior behind explicit repair/reindex commands.
