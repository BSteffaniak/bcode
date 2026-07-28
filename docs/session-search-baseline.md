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

## Baseline measurements

Release-development measurements captured on 2026-07-28 with the ignored deterministic benchmark:

```sh
cargo test -p bcode_session benchmark_session_investigation_reads_across_store_sizes \
  --lib -- --ignored --nocapture
```

The generated current-format stores use synthetic alternating user/assistant messages. Listing
measures the catalog call, paging requests the latest 100 canonical events, export decodes complete
canonical history, and selected-export search writes JSONL then runs fixed-string `rg -c` over that
explicit export. Results are development-host reference points, not universal deadlines:

| Profile | Events | List | Latest page (100) | Complete export | Export bytes | Selected-export `rg` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Small | 100 | 88 µs | 3.696 ms | 1.909 ms | 25,978 B | 14.188 ms |
| Medium | 5,000 | 33 µs | 37.651 ms | 101.654 ms | 1,324,229 B | 499.206 ms |
| Large | 50,000 | 72 µs | 305.283 ms | 1,268.794 ms | 13,391,732 B | 5,699.566 ms |

The benchmark's first run showed the same scaling shape; the table records the final run after the
selected-export search was changed to invoke `rg` over actual generated JSONL.

These results confirm:

* catalog listing is independent of event count in this controlled already-derived setup;
* current canonical paging still grows materially with total event count despite a 100-event result
  bound, so indexed/sidecar-backed bounded access remains important;
* complete export and grep are appropriately explicit, but unsuitable as ordinary cross-session
  search paths;
* no indexed/deep provider numbers exist yet and none should be inferred from export scanning.

## Representative benchmark corpus

Performance and correctness work uses deterministic corpus classes rather than arbitrary personal
sessions:

* **Small:** 100 canonical events for command overhead and correctness smoke tests.
* **Medium:** 5,000 canonical events for routine long-session behavior.
* **Large:** 50,000 canonical events for bounded-read scaling and explicit-export cost.
* **Transcript mix:** titles plus alternating user/assistant/system content, repeated terms, phrases,
  Unicode, empty/short messages, and very long lines.
* **Reasoning mix:** finalized structured reasoning activities and legacy finalized reasoning text,
  with transport deltas present as explicit exclusions.
* **Terminal mix:** SGR/CSI styling, OSC titles and hyperlinks, CRLF/lone-CR progress replacement,
  backspaces, tabs, control bytes, invalid UTF-8 boundaries, huge lines, repeated build output, and
  errors at the beginning, middle, and end.
* **Tool mix:** shell commands, bounded stdout/stderr, JSON arguments/results, failed and cancelled
  lifecycle events, generic plugin results, artifacts/references, permission decisions, and runtime
  work.
* **Sensitivity mix:** token/password-like strings, credential-bearing command examples, private
  paths, and provider errors used only to verify projection exclusion/redaction and secret-safe
  diagnostics.
* **Durability mix:** current, future-version, malformed, sequence-inconsistent, stale-projection,
  repair-required, and provider-index-corrupt states.

Generated fixtures must remain deterministic and synthetic. Personal canonical sessions may provide
separate diagnostic evidence but are not checked in or treated as reproducible benchmark inputs.
Search-provider acceptance must report results by corpus class and content policy, including source
bytes, normalized bytes, indexed bytes, exclusions, truncation, freshness, and query completeness.

## Initial acceptance budgets

Use these as implementation acceptance targets until release hardware measurements justify an
explicit revision:

* catalog/session listing: p95 at most 25 ms for 50,000-event sessions, without canonical replay;
* bounded latest/around/inspection read: p95 at most 100 ms for a 100-event result from a
  50,000-event session;
* ordinary indexed transcript query: p95 at most 100 ms for a 20-hit response over 100,000 events;
* incremental search freshness: p95 at most 2 seconds after canonical append when an enabled local
  provider is healthy;
* daemon startup overhead: at most 25 ms p95 with search disabled and no provider worker/index open;
* ordinary query coordinator memory: at most 64 MiB incremental RSS excluding provider-owned
  configured writer caches;
* default derived search storage: at most 50% of indexed normalized source bytes and at most 2 GiB,
  whichever limit is reached first;
* deep compressed-output search: no universal latency promise initially, but it must be cancellable,
  deadline-bounded, and report incomplete coverage rather than block ordinary results.

The 50,000-event page baseline currently exceeds the 100 ms target. That is recorded as a known gap,
not relaxed into the budget. Provider index/corpus amplification must be measured separately before a
backend is accepted.
