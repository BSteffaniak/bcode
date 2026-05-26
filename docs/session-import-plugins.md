# Session Import Plugins

Bcode session import providers expose external coding-agent conversations as one-time imports into native Bcode sessions.

## Interface

Plugins implement the service interface:

```text
bcode.session_import/v1
```

Operations:

* `list_sources` returns source metadata such as `pi` or `claude-code`.
* `discover_sessions` returns lightweight external session summaries for picker/catalog views.
* `load_session` returns one selected external session as normalized import events.

The shared Rust contract lives in:

```text
packages/session-import
```

## Ownership rules

* Import plugins read source-owned files read-only.
* Import plugins do not write Bcode session event logs.
* The Bcode server owns duplicate detection, provenance metadata, and native session creation.
* Imported sessions are one-way snapshots. After import, Bcode does not resync that session from the source.

## Privacy expectations

* Do not recursively scan arbitrary home directories.
* Scan known app data roots or explicitly configured paths only.
* Surface source IDs and scanned roots through config/docs.
* Do not preserve raw external events by default.

## Normalized events

Providers should map source events into `ImportableSessionEventKind`:

* `UserMessage`
* `AssistantMessage`
* `ToolCallRequested`
* `ToolCallFinished`
* `ModelChanged`
* `AgentChanged`
* `ContextCompacted`
* `SystemMessage`

When source data cannot be represented exactly, emit an `ImportWarning` and, if useful for transcript continuity, a visible `SystemMessage`.
