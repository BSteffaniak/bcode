# Session Search Baseline Evidence and Read-Surface Inventory

## Lock/read failure reproduction

Captured on 2026-07-28 while normal Bcode processes were active.

Active default state root:

```text
/Users/braden/.local/state/bcode
```

Selected canonical session:

```text
d4bc4495-89f6-4bcd-ac64-1b58d5642992
```

Canonical database path involved in the current failure:

```text
/Users/braden/.local/state/bcode/sessions/d4bc4495-89f6-4bcd-ac64-1b58d5642992/session.db
```

`lsof` showed Bcode PID `88518` holding the corresponding `session.db-wal`. A stock SQLite
read happened to complete in approximately 0.02 seconds and reported 5,115 events, but the supported
installed command:

```sh
bcode session history d4bc4495-89f6-4bcd-ac64-1b58d5642992
```

failed after approximately 4.23 seconds with:

```text
server returned error session_unavailable: failed to initialize database connection:
Locking error: Failed locking file '.../session.db-wal'. File is locked by another process
```

This demonstrates the architectural problem precisely: callers cannot rely on acquiring another
storage-engine connection, and even a supported command routed to a different compatible daemon can
collide with the process currently owning the WAL. The fact that stock SQLite happened to read the
file is not a supported solution; it bypasses Bcode's schema, compatibility, bounded-read, and
canonical-owner semantics.

The implementation direction is therefore daemon/application-owned reads and eventual search-record
projection, not more direct-reader workarounds.

## History-skill persistence dependency inventory

Before migration, `/Users/braden/.config/bcode/skills/bcode-session-history/SKILL.md` contained:

* 14 direct `sqlite3` invocations;
* direct references to `sessions/catalog.db` and per-session `session.db`;
* recursive `find ... -name session.db` discovery;
* direct payload `LIKE` searches over every discovered event table;
* direct reads of `events`, transcript, input-message, tool-run, runtime-work, and session-state
  tables;
* troubleshooting fallbacks that inspected canonical database paths when supported commands failed;
* broad-search guidance that treated private storage layout and projection schemas as a user-facing
  interface.

Those dependencies are the behavior to remove from the skill. WAL/lock handling was implicit rather
than a reliable protocol: direct tools were expected to contend with whichever Bcode process owned
the files.

## Current read-surface inventory

### Session domain (`packages/session`)

| Surface | Classification | Notes |
| --- | --- | --- |
| `session_history_page` | Bounded normal read | Hard event limit; canonical cursor and compatibility diagnostics. |
| `session_history_around` | Bounded normal read | Stable sequence anchor, source bounds, and anchor-presence status. |
| `session_inspection_page` | Bounded typed read | Event-type prefilter, bounded candidate decoding, typed semantic categories. |
| `session_events_range` | Private bounded domain read | Used by projections/forks; not the public investigation API. |
| `session_projection_window` | Bounded semantic runtime read | Transcript/application projection behavior, not general forensic search. |
| `session_history` | Explicit complete history | Export/debug only; never a normal UI/search hydration path. |
| `session_health` and diagnosis/doctor services | Bounded/non-mutating diagnosis | Surfaces compatibility, damage, ownership, and projection state. |
| repair/reindex/migration APIs | Explicit maintenance | Mutating/full-replay behavior requiring ownership and safety checks. |

### Server and IPC (`packages/server`, `packages/ipc`)

| Surface | Classification | Notes |
| --- | --- | --- |
| `SessionHistoryPage` | Portable bounded normal read | Routed through server namespace/compatibility checks. |
| `SessionHistoryAround` | Portable bounded normal read | Canonical hit hydration. |
| `SessionInspection` | Portable bounded typed read | Failed tools, permissions, selections, runtime work, compactions, terminal outcomes. |
| `SessionHistory` | Explicit complete history | Full payload response for export/debug; may be expensive. |
| session diagnosis/open/doctor responses | Portable diagnosis | Explicit degraded/repair/compatibility outcomes. |

### Client (`packages/client`)

* `session_history_page`: bounded canonical paging.
* `session_history_around`: bounded sequence-neighborhood hydration.
* `session_inspection`: bounded typed semantic inspection.
* `session_history`: explicit complete history.
* `session_artifact_range`: bounded artifact byte retrieval where supported.

### CLI (`packages/cli`)

| Command | Classification |
| --- | --- |
| `bcode session list --json` | Bounded metadata/catalog read. |
| `bcode session history ... --limit ... --json` | One bounded canonical page. |
| `bcode session around ... --json` | Bounded sequence-neighborhood hydration. |
| `bcode session inspect ... --json` | Bounded typed semantic inspection. |
| `bcode runtime-work history ... --limit ...` | Bounded runtime-work view. |
| `bcode session export ... --format jsonl` | Explicit complete selected-session export. |
| `bcode session timeline` | Complete-history debugging presentation; not a normal search path. |
| `bcode session diagnose/doctor` | Read-only diagnosis. |
| `bcode session repair/reindex` | Explicit mutating maintenance. |

Direct database opens remain appropriate only inside explicit diagnosis/doctor/repair/reindex/migration
implementation or tests. Ordinary CLI list/history/around/inspection and search workflows must not
use them.

## Supported investigation coverage

Current supported operations can:

* list sessions with stable IDs and machine-readable summaries;
* page canonical history in either direction;
* hydrate context around one canonical sequence;
* inspect failed tools, permission decisions, model/reasoning/agent/cwd changes, runtime work,
  compactions, and terminal outcomes;
* inspect durable runtime-work history;
* produce read-only diagnosis/doctor results;
* export one explicitly selected complete session.

Current supported operations cannot yet:

* perform indexed full-text search across sessions;
* search reasoning, messages, and normalized large tool/shell output through provider routes;
* resolve all trace/blob references through one dedicated portable investigation command;
* report federated provider coverage, freshness, exclusions, or partial failures because the search
  contract/coordinator does not yet exist.

## Baseline measurement status

Lock failure timing above is recorded. Representative small/medium/large measurements for listing,
paging, complete export, and eventual indexed/deep search remain pending. Those measurements require
a controlled corpus and current binaries that include the new bounded CLI contracts; they should not
be fabricated from the installed older binary.
