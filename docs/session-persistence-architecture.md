# Session Persistence Architecture

## Canonical storage and authority

A Bcode session id maps to exactly one canonical database:

```text
<state-dir>/sessions/<session-id>/session.db
```

The `events` table in that database is the authoritative ordered session history. Canonical event
rows are append-only and sequence-contiguous. Writer epochs, daemon namespaces, protocol versions,
and build fingerprints never select a different session root, directory, database, or history.
They are compatibility and routing metadata only.

Other authoritative session-owned state, such as composer drafts, lives in explicitly designated
tables in the same per-session database. It is authoritative for that state but is not transcript
history.

The default production session root is resolved only by
`bcode_config::default_session_store_dir()`. Low-level session APIs accept an explicit root for
tests, imports, and isolated stores, but all production default paths use the same canonical root.
`bcode_session::db::session_dir_path` and `session_db_path` own per-session path construction.

## Live-only progress boundary

Intermediate provider argument fragments, request previews, execution frames, and replaceable progress
are not session facts. They use `SessionLiveEvent` and are retained only in bounded in-memory
registries while the owning turn or invocation is active. They never receive a durable event
sequence and must not be written to `session.db`, projection indexes, catalogs, manifests,
finalized artifact metadata, trace blobs, workflow state, crash reports, or structured log fields.
Daemon restart intentionally discards this state.

The durable boundary begins with complete semantic facts: a validated tool request, required
permission request/resolution, lifecycle facts that remain meaningful after reload, and the terminal
result or finalized artifact. Session append validation rejects transient contributions and any new
durable contribution with `Progress` placement. Persistence decoding retains historical durable or
unplaced contributions for compatibility, but those decoded paths are not writable extension
points.

```text
provider/plugin intermediate update
  -> bounded server live-state registry
  -> SessionLiveEvent checkpoint/delta
  -> SessionView transient projection
  -> renderer-native presentation

complete request / permission / terminal result
  -> SessionEvent append
  -> canonical session.db history and durable projections
```

Reconnect may restore only the current bounded active checkpoint. It does not replay missed live
deltas. Completing provider argument assembly does not retire a result-targeted request preview: the
bounded preview remains active through permission and execution so reconnect can restore it. The
server first persists and publishes the complete `ToolInvocationResultRecorded` semantic fact, then
retires the live preview. Cancellation, failure, timeout, retry/supersession, or host-owned teardown
that cannot produce a canonical result removes active state directly. Daemon restart intentionally
drops every preview while durable request/result history remains authoritative.

## On-disk layout

```text
<state-dir>/sessions/
  <session-id>/
    session.db             # authoritative database
    session.db-wal         # database implementation sidecar
    session.db-shm         # database implementation sidecar
    manifest.json          # derived discovery/display cache
  catalog.db               # legacy derived catalog cache
  catalogs/
    <build-namespace>/
      catalog.db           # derived catalog cache
  locks/                   # cross-process coordination
  leases/                  # live compatibility-owner metadata
```

Classification:

* `session.db` is authoritative for canonical events and session-owned database state.
* WAL/SHM files are database implementation sidecars and must be handled through Bcode's Turso
  repair path.
* Materialized projection tables are derived from canonical events and may be rebuilt only by a
  controlled migration, reindex, or repair operation.
* `manifest.json` and every catalog database are disposable discovery caches. Missing, stale, or
  build-scoped catalog state must never hide a canonical session directory or replace canonical
  event history.
* Lock and lease files are coordination metadata, not session history.

## Catalog discovery

Catalog discovery is best-effort, bounded, and non-mutating:

* Enumerate UUID-shaped directories directly under the canonical session root.
* Merge namespaced catalog rows, legacy catalog rows, and manifests as display caches. Known legacy
  manifest schemas remain eligible for bounded display discovery; unknown future schemas and
  inconsistent identities fail closed without opening the session database.
* Ensure every directory containing `session.db` remains represented even when all caches are
  missing or stale.
* Do not run schema migration, projection rebuild, repair, or full canonical replay while listing.
* A damaged session should remain visible with degraded or repair-required state rather than
  disappearing from the catalog.

Build-namespaced catalogs may coexist because they are rebuildable caches. They do not create
build-specific session storage and cannot choose which session history is opened.

## Session database open modes

Per-session access is split by capability:

* `open_existing_turso_in_root` opens an existing database without creating directories, running
  DDL, or rebuilding projections.
* `initialize_turso_in_root` creates a new database with the complete current schema and refuses to
  overwrite an existing database.
* `migrate_turso_in_root` requires borrowed `SessionMaintenanceGuard` and `SessionWriteGuard`
  capabilities. Schema migration, full derived-projection replay, validation, and writer-contract
  advancement commit in one transaction.
* Compatibility `open_turso*` entry points initialize only missing databases. Existing databases
  use the non-migrating open.

Health, doctor, diagnosis, catalog, and audit paths remain non-migrating. A normal session-manager
load may migrate only storage classified as known legacy after it acquires exclusive per-session
maintenance ownership. All other maintenance remains explicit.

## Current runtime and migration boundaries

`bcode_session` owns only the current writer contract, current event model, current projections,
and bounded current-session reads and writes. Historical payload DTOs, released writer-epoch
planning, historical classification, and conversion belong to the dedicated
`bcode_session_migration` domain crate. Historical DTOs must not leak into session models, client
protocols, or runtime APIs.

The intended dependency graph is one-way:

```text
server composition -> bcode_session_migration -> bcode_session migration-target API
server composition -> bcode_session
client/TUI -> typed session-open operation models
```

`bcode_session` must not depend on `bcode_session_migration` in the final architecture. The current
migration-target API is deliberately narrow: bounded canonical pages, transactional canonical-row
replacement, current projector ingestion/finalization, strict validation, and final writer-contract
advancement. The migration service owns source classification, epoch planning, historical decoding,
backup evidence, operation progress/terminal state, and orchestration. The server composes the
migration service with current session loading and ownership; clients and the TUI only observe typed
operation snapshots.

A migration plan records every monotonic released epoch edge for audit, but canonical history is
normalized directly to the final current representation in one bounded traversal. It is not replayed
once per epoch edge or once per projection. Runtime canonical rows remain append-only; the sole
encoding-rewrite exception is an explicit, exclusively owned writer-epoch migration transaction.
That transaction preserves event sequence, timestamp, session identity, provenance, ordering, and
semantics while rebuilding every current derived projection.

Migration order is mandatory:

1. classify the source without mutation;
2. refuse migration while any live runtime owner exists;
3. acquire the maintenance coordinator and then the maintenance write lock;
4. create and verify a retained backup and its evidence manifest;
5. begin the database transaction;
6. normalize canonical events and rebuild projections in bounded pages;
7. reject unresolved compatibility issues and validate all current write-readiness invariants;
8. write the migration receipt;
9. advance the writer epoch as the final transactional mutation;
10. commit and atomically adopt the corrected runtime lease before publishing terminal `Ready`.

The receipt records source and target epochs, stable ordered migration-step IDs, source and target
ordered payload digests, event count, converted/retired-known counts, backup identity, and completion
time. Backup evidence records source identity, source/target writer contracts, copied database and
sidecars, byte counts, and verification status. Receipts are current audit metadata, not runtime
branches for old behavior.

Corrected runtime ownership is one daemon instance per session, with any number of clients routed
through that owner. Lease metadata includes writer epoch and daemon instance identity. A live older
daemon is never forced out or migrated underneath: it continues until release, idle close, graceful
stop, or process exit. A newer daemon waits or reports actionable owner metadata without taking the
database lock. Maintenance-to-runtime handoff must have no unowned writable gap, and an older writer
must not reacquire after epoch advancement.

## Automatic known-legacy migration

Normal first load is serialized per session. The manager inspects the migration ledger and durable
storage contract without mutation, then follows one of these paths:

* Current storage: acquire a compatible runtime lease and recheck compatibility while ownership is
  held.
* Known legacy storage: acquire exclusive maintenance ownership and the maintenance write lock,
  reopen and reclassify, migrate atomically, validate write readiness, and transition maintenance
  ownership directly into a compatible runtime lease.
* Unknown migration ids, dirty or failed migration records, future writer epochs, unsupported
  contract schemas, malformed canonical history, or ledger/contract inconsistencies: fail closed
  without mutation.

Automatic known-legacy migration acquires exclusive maintenance ownership and the maintenance write
lock, creates and byte-verifies a retained copy of the complete session directory, and only then
calls `migrate_turso_in_root`. Migration strictly validates contiguous canonical
sequences and session identity, preserves canonical events and drafts, rebuilds all required derived
projections through the same projector functions used by normal append, verifies checkpoints at the
canonical tail, and updates the writer contract only when validation succeeds. Any live session
owner blocks migration. Unknown, future, dirty, ambiguous, or corrupt storage still fails closed.

## Durable writer contract

`session_storage_contract` contains a singleton versioned writer epoch. Mutation-capable processes
advertise their epoch in session leases and validate the durable row before mutation. The current
contract-aware baseline is epoch `5`. Epochs `3` and `4` are recognized legacy storage and migrate
under exclusive maintenance ownership by rebuilding all required projections. That rebuild applies
terminal tool lifecycle events to the transcript and tool-run projections, so an invocation that
finished before its result record was persisted no longer remains falsely `running`. The
session-compatibility projection records its canonical-tail checkpoint and any opaque event
sequence/kind/schema issues; a current projection with issues is inspectable but read-only.

A known pre-contract migration prefix with no contract table/row is legacy epoch `1`. A missing
contract after the migration ledger says contract initialization completed is inconsistent and
repair-required. Future epochs are never downgraded or automatically migrated.

The writer epoch must change whenever an older writer could no longer preserve canonical append
atomicity or required projections. Epoch values govern compatibility, never filesystem location.

## Lock order and ownership

The required lock order is:

1. session maintenance coordinator;
2. session write lock;
3. database connection/transaction.

Ordinary compatible writers share maintenance coordination and serialize write critical sections.
Mutating maintenance holds the coordinator exclusively, refuses every live owner, and then acquires
the write lock. Never acquire these capabilities in reverse order.

A loaded actor retains its compatibility lease while dropping idle database/event caches. This
prevents an incompatible writer from claiming the session between operations.

## Historical epoch-root recovery

An earlier, reverted implementation briefly wrote sessions beneath:

```text
<state-dir>/session-storage/writer-epoch-2/
```

Only `bcode_session::legacy_storage` may recognize this exact historical path. It is migration input,
never an active session store.

Recovery rules:

* Never open the historical root through normal `SessionManager` access.
* Relocate a complete session directory atomically only when no live owner exists and the canonical
  destination is absent.
* Never merge, overwrite, or silently choose between duplicate historical and canonical sessions.
* Report live-owner blocks and destination conflicts for diagnosis.
* Remove empty historical coordination/root directories after successful relocation.
* Repeated recovery is idempotent.

## Canonical append and projections

A canonical append and all required projection updates are one transaction. Before insertion, the
append path validates:

* the durable writer epoch;
* the next contiguous event sequence;
* every required projection schema;
* every required projection checkpoint against the prior canonical tail.

Each projector advances only its own checkpoint after its projection update succeeds. Missing,
stale, incompatible, or discontinuous required projections reject and roll back the append.

Required projections include current session state, input history, transcript spans, tool runs,
artifact references, runtime work, request-context occupancy, model context, and turn receipts.
Normal reads never silently rebuild them.

## Normal bounded reads

Normal attach and history paths use database projections and bounded range queries. They do not full
replay canonical events or invoke repair. Full history remains available for explicit export,
diagnosis, and maintenance.

Model context begins at the latest valid local or provider compaction boundary and reads the current
projection. Missing, stale, incompatible, or corrupt projections remain repair-required. Rebuilds
and known legacy migrations run only through explicit maintenance commands.

## Repair and maintenance

Maintenance commands are explicit:

* `bcode session export <session-id>` is the pre-cutover escape hatch. It performs a read-only
  strict canonical-event JSONL export through the non-migrating existing-database open and does not
  load the runtime session, acquire write ownership, migrate, repair, or reindex storage.
* `bcode session diagnose <session-id>` reports writer, projection, canonical-tail, and ownership
  state without mutation.
* `bcode session doctor` diagnoses database and WAL state without mutation.
* `bcode session repair` acquires exclusive maintenance ownership, creates backups, and performs
  only supported database-sidecar/tail repair.
* `bcode session reindex` acquires maintenance and write capabilities before rebuilding projections.

Repair uses Bcode's native Turso stack. Stock SQLite checkpoint/repair is not the primary recovery
path. Catalog listing, picker display, normal attach, and paged history must never invoke repair.

### Expected migration duration

Release-acceptance measurements on 2026-05-10 used a debug test build on the current development
host. The generated stores and command are deterministic; these measurements are diagnostic
reference points rather than universal deadlines:

| Events | Final storage | DB growth | Final WAL | Peak RSS | Total | Backup copy | Backup verify | Reprojection | Commit | WAL checkpoint | Readiness |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | 237,568 B | 233,472 B | 0 B | 220,659,712 B | 196 ms | 2 ms | 1 ms | 143 ms | 0 ms | 1 ms | 1 ms |
| 5,000 | 4,030,464 B | 4,026,368 B | 0 B | 221,396,992 B | 11,153 ms | 6 ms | 5 ms | 10,676 ms | 19 ms | 0 ms | 26 ms |
| 50,000 | 40,046,592 B | 22,851,584 B | 0 B | 221,003,776 B | 171,365 ms | 132 ms | 106 ms | 154,391 ms | 505 ms | 8 ms | 564 ms |

Commands:

```text
BCODE_MIGRATION_BENCHMARK_PROFILE=small cargo test -p bcode_session benchmark_generated_legacy_session_migrations -- --ignored --nocapture
BCODE_MIGRATION_BENCHMARK_PROFILE=medium cargo test -p bcode_session benchmark_generated_legacy_session_migrations -- --ignored --nocapture
BCODE_MIGRATION_BENCHMARK_PROFILE=large cargo test -p bcode_session benchmark_generated_legacy_session_migrations -- --ignored --nocapture
```

The same host measured current-session preparation over 100 runs at 5,768 µs median, 6,433 µs
p95, and 6,911 µs maximum. The enforced release gate is p95 at or below 25 ms:

```text
cargo test -p bcode_session benchmark_current_session_preparation_latency -- --ignored --nocapture
```

The table's peak RSS is `/usr/bin/time -l` maximum resident set size for the complete isolated test
process, so it is a conservative process high-water mark rather than migration-only allocation.
Across these profiles it remained effectively flat (220.7–221.4 MB). DB growth and peak RSS are
now measured, and explicit post-commit checkpointing leaves the final WAL at zero bytes for every
profile. No release limits are set until the bounded-current-open regression below is corrected.
Switchy's Turso `Database::exec_raw` rejects row-producing pragmas with `unexpected row during
execution`; `Database::query_raw("PRAGMA wal_checkpoint(TRUNCATE)")` is therefore the supported
abstraction and is timed separately. A direct current-store event-count probe measured preparation
p95 at 6,370 µs for 100 events, 35,540 µs for 5,000 events, and
469,714 µs for 50,000 events. This fails the bounded-current-open acceptance requirement and is not
a release gate; current preparation must stop doing event-count-proportional work before that item
can be closed.

Storage speed, canonical payload size, projection mix, retained sidecars, and host load all affect
elapsed time. During healthy work, the TUI reports the active stage and stage-local natural units. A
large session can spend most of its time rebuilding indexes. Investigate when progress stops
changing rather than treating these host-specific figures as a timeout. Capture
`bcode session diagnose <session-id>` and `bcode server status --verbose` for support; metrics
operators should compare `session.migration.*_duration_ms` stage histograms to total migration
duration.

### Migration troubleshooting

Start every investigation with the full session UUID:

```text
bcode session diagnose <session-id>
bcode server status --verbose
```

The diagnosis command is read-only. Preserve the complete session directory and every reported
retained backup before running maintenance.

* **Waiting for ownership:** Another daemon or client may still own the session. Inspect the verbose
  server status, close the owning client normally, and wait for ownership to clear. Do not delete
  lease files or terminate a process solely to bypass ownership checks. If ownership remains after
  all owners have exited, run `bcode session doctor <session-id>` before considering repair.
* **Backup failure:** Migration does not begin mutation until backup verification succeeds. Check
  the reported filesystem error, free space, destination permissions, and destination conflicts,
  then retry normal open. Keep any retained backup path reported by Bcode. Do not replace the
  session database with a partial backup.
* **Structural corruption or repair-required:** Malformed canonical JSON, sequence gaps, session-id
  mismatches, dirty migration history, and stale/incompatible projections fail closed. Capture
  `bcode session diagnose <session-id> --json`, then use `bcode session doctor <session-id>` and a
  supported `repair` or `reindex` command when its diagnosis recommends one. Never edit the
  database or WAL directly.
* **Degraded/read-only completion:** Trustworthy but unsupported event semantics remain visible
  through bounded history while writable attach is disabled. Preserve the backup and diagnosis
  output, then upgrade to a Bcode build that understands the reported event/schema. Repair must not
  discard or reinterpret opaque events merely to make the session writable.

A stalled or failed migration must remain visible as a classified terminal state. Retrying,
leaving the picker, or disconnecting an observer must never silently downgrade the failure or
remove its retained backup path.

## Finalized artifact references

Finalized plugin artifacts are resolved through the `artifact_references` projection, keyed by
artifact id and reference key. It stores generic producer/schema identity, storage URI, content
type, projected length, availability/completeness, checksum, and finalizing event sequence.

Artifact range reads use this projection and bounded file ranges. The projection checkpoint must
equal the canonical event tail. Missing/stale projection state surfaces `ProjectionStale`; malformed
state surfaces repair-required. Relative and supported legacy absolute/file references are accepted
only after canonical path confinement beneath the session artifact root.

## Non-negotiable invariants

* A session id has one canonical database path.
* Writer epoch and build identity never choose storage location.
* The `events` table is canonical history.
* Catalogs, manifests, projections, and in-memory state are derived.
* Catalog damage cannot hide canonical session directories.
* Normal reads do not migrate, repair, or full replay.
* Known legacy migration requires exclusive maintenance ownership and may run only during first real
  session load.
* Unknown, future, dirty, ambiguous, or corrupt storage fails closed.
* Historical duplicate roots are never merged automatically.
