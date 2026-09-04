# Session Persistence Architecture

## Canonical storage and authority

A Bcode session id maps to exactly one canonical database within the state location that owns it:

```text
<sessions-root>/<session-id>/session.db
```

`<sessions-root>` is the canonical session store root of one resolved state location. See
[State Locations](state-locations.md) for selection precedence. Exactly one state location owns a
session's canonical storage, and the path within that location derives only from the session ID.

The `events` table in that database is the authoritative ordered session history. Canonical event
rows are append-only and sequence-contiguous. Writer epochs, daemon namespaces, exact produced-artifact
identities, protocol versions,
and build fingerprints never select a different session root, directory, database, or history.
They are compatibility and routing metadata only. Exact artifact identity chooses a matching daemon
endpoint, never a different session database. Multiple artifact versions therefore share canonical
session storage while runtime leases prevent conflicting ownership.

A state location is likewise not selected by writer, build, process, or frontend identity, and an
unavailable or unverifiable location is never resolved to a substitute location: substituting one
would manufacture a second canonical storage path for the same session IDs. Aggregated discovery may
span locations, but it confers no authority — a session's mutations, ownership, and repair apply only
to the location that owns its canonical storage. When more than one location claims the same session
ID, the conflict is surfaced and no location is opened as authoritative until an explicit maintenance
operation resolves it.

Other authoritative session-owned state, such as composer drafts, lives in explicitly designated
tables in the same per-session database. It is authoritative for that state but is not transcript
history.

The default production session root is resolved only by
`bcode_config::default_session_store_dir()`. Low-level session APIs accept an explicit root for
tests, imports, and isolated stores, but all production default paths use the same canonical root.
`bcode_session::db::session_dir_path` and `session_db_path` own per-session path construction.

### Turn execution options

Each admitted user turn persists its provider-neutral `TurnExecutionOptions` inside the canonical
`user_message` event before scheduling. The nested execution-options schema is versioned
independently because adding a defaulted option does not alter the outer event kind or binary enum
layout. Version 4 adds `permission_mode`; payloads from supported earlier versions decode the
missing field as `enforce`, while a bypass value paired with an earlier nested version is rejected.
This additive nested change does not advance the session event schema or writer epoch: current
writers still preserve every canonical event and older current-format turns retain an unambiguous
safe meaning. No read-path migration, repair, or replay is needed.

Launch-selected permission and tool modes are therefore ephemeral only until admission. Once a
turn is admitted, queued execution and supported recovery use the canonical event value rather than
client connection, renderer, environment, configuration, or daemon-global state.

## Positioned durable transcript events

Session event schema 42 and writer epoch 6 introduced positioned durable assistant segments,
reasoning activities, and tool requests. The corrected model-context projection uses schema version
3 so affected sessions require explicit `bcode session reindex` before additional turns. Bounded
transcript attach now selects directly from a bounded canonical tail instead of trusting the obsolete
derived transcript index, remaining bounded and non-mutating while explicit reindex can rebuild the
index. `TurnOutputPosition` values place different output kinds in one application-turn semantic
ordering domain after host rebasing. Durable completion revises the matching live semantic identity
instead of creating an arrival-ordered replacement row. Epoch-5 history remains readable as legacy
sequence-ordered input; migration does not invent output positions that were never recorded.

This durable ordering does not make live checkpoints durable. Ordered assistant/reasoning appends,
active presentations, and terminal tombstones remain bounded actor-owned state as described below.

## Live-only progress boundary

Intermediate provider argument fragments, request previews, execution frames, replaceable progress,
and ordered assistant/reasoning text appends, checkpoints, revisions, and terminal tombstones are not
session facts. They use `SessionLiveEvent` and are retained only in bounded in-memory registries while
the owning turn or invocation is active. They never receive a durable event sequence and must not be
written to `session.db`, projection indexes, catalogs, manifests, finalized artifact metadata, trace
blobs, workflow state, crash reports, or structured log fields. Daemon restart intentionally discards
this state; attach within the same live process may hydrate bounded checkpoints, but those checkpoints
are state transfer only and do not imply durable or reconnect-safe resume.

Completed assistant response segments and completed structured reasoning activities cross the durable
boundary as correlated semantic facts. Their durable events finalize or replace matching live items
without promoting partial live text, guessing missing bytes, or persisting intermediate operations.
The durable boundary begins with complete semantic facts: a validated tool request, required
permission request/resolution, lifecycle facts that remain meaningful after reload, a complete
assistant response segment, and the terminal result or finalized artifact. A complete assistant
segment persists its application turn ID, stable turn-local segment ID/order, and complete text so
live finalization, bounded replay, and fresh replay use the same semantic identity. Append,
revision, offset, checkpoint, truncation-window, and live tombstone state remain replaceable
progress and never cross this durable boundary. Session append validation rejects transient
contributions and any new durable contribution with `Progress` placement. Persistence decoding
retains historical durable or unplaced contributions for compatibility, but those decoded paths
are not writable extension points.

```text
provider/plugin intermediate update
  -> bounded server live-state registry
  -> SessionLiveEvent checkpoint/delta
  -> SessionView transient projection
  -> renderer-native presentation

complete assistant segment / request / permission / terminal result
  -> SessionEvent append
  -> canonical session.db history and durable projections
```

Reconnect may restore only the current bounded active checkpoint. It does not replay missed live
deltas. Daemon restart therefore discards incomplete assistant/reasoning text rather than promoting
it to a durable message; only already-committed complete assistant segments survive. Completing
provider argument assembly does not retire a result-targeted request preview: the bounded preview remains active through permission and execution so reconnect can restore it. The
server first persists and publishes the complete `ToolInvocationResultRecorded` semantic fact, then
retires the live preview. Cancellation, failure, timeout, retry/supersession, or host-owned teardown
that cannot produce a canonical result removes active state directly. Daemon restart intentionally
drops every preview while durable request/result history remains authoritative.

## On-disk layout

```text
<sessions-root>/
  <session-id>/
    session.db             # authoritative database
    session.db-wal         # database implementation sidecar
    session.db-shm         # database implementation sidecar
    manifest.json          # derived discovery/display cache
  session-artifacts/
    <session-id>/          # session-owned tool artifacts, sibling of canonical storage
  catalog.db               # global summary cache and draft-session state
  catalogs/
    <build-namespace>/
      catalog.db           # retired summary cache and possible draft-session state
  locks/                   # cross-process coordination
  leases/                  # live compatibility-owner metadata
```

`<sessions-root>` belongs to one resolved state location and may be placed on a different volume from
the state root. Non-session durable state — the daemon registry, daemon images, logs, settings,
runtime permissions, workflows, traces, and derived data — stays under `<state-root>` and is not moved
by a session-root override. See [State Locations](state-locations.md).

Session artifacts are a sibling of the canonical `<session-id>/` directory rather than nested inside
it, because migration backup walks the canonical directory recursively and nesting bulk artifact bytes
would make every canonical backup copy them. Canonical discovery only accepts directory names that
parse as a session ID, so the named sibling is ignored by catalog scans.

Classification:

* `session.db` is authoritative for canonical events and session-owned database state.
* WAL/SHM files are database implementation sidecars and must be handled through Bcode's Turso
  repair path.
* Materialized projection tables are derived from canonical events and may be rebuilt only by a
  controlled migration, reindex, or repair operation.
* `manifest.json` and catalog `sessions` rows are disposable discovery/display caches. Missing,
  stale, or build-scoped summary state must never hide a canonical session directory or replace
  canonical event history.
* Catalog `composer_drafts` rows are authoritative for draft-session composer state. A catalog
  database therefore cannot be deleted as a wholly disposable cache until maintenance has
  preserved any authoritative draft rows it contains.
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

Build-namespaced catalogs may coexist because their summary rows are rebuildable caches. They do not
create build-specific session storage and cannot choose which session history is opened. Any
retired catalog database must still be treated as potentially containing authoritative
`composer_drafts` rows until explicit maintenance has inspected and preserved them.

## Retired catalog namespaces

`bcode session retired-catalogs` inventories directories under `sessions/catalogs/` without
mutation. Reports include namespace classification, daemon-evidence disposition, catalog/WAL/SHM
sizes, authoritative draft count, proposed action, and errors. `--apply` is explicit maintenance.

Cleanup fails closed when matching artifact/build/namespace daemon evidence is live or ambiguous,
or when any registry record is malformed. It rechecks that evidence after acquiring the session
catalog maintenance lock. The retired and active catalogs are opened through typed `Database`
operations; retired draft rows are copied only when their `updated_at_ms` is strictly newer than the
active row. Equal or older retired drafts are reported as conflicts and skipped. Both databases are
closed through `Database::close()` before the complete namespace directory is removed. Individual
WAL/SHM files are never removed independently. Repeating cleanup after successful removal is an
idempotent empty inventory.

## Generic session derivation

Current-format session branching is implemented as generic session derivation rather than a
fork/clone persistence API. A caller captures a bounded source snapshot and submits a versioned
request containing an exact generation, inclusive cutoff, idempotency identity, optional initial
composer draft, and producer-owned namespaced lineage.

The session layer reads canonical source events in bounded ascending pages and writes each bounded
page with one transaction that updates canonical events and every affected projection together.
The destination is built beneath `.derivation-staging/<operation-id>/`, outside catalog and normal
session discovery. After canonical/projection validation, the session database, manifest, and
staging directory are synchronized; the complete session directory is then atomically renamed to
its one canonical path. Catalog publication and actor adoption occur only after that rename.

Versioned operation receipts under `.derivation-operations/` hold request fingerprints, monotonic
progress, destination identity, and immutable terminal outcomes. They are operation coordination,
not canonical transcript authority. Identical retries return the same terminal result; conflicting
duplicates fail closed. Explicit bounded housekeeping may remove only staging owned by nonterminal
receipts and never removes canonical destinations or terminal receipts.

Cancellation is checked before reads, after bounded writes, before finalization, and immediately
before publication. Cancellation and failures remove operation-owned staging and cannot expose a
partial destination. Historical `session_forked` events are not a current session event and remain
classified only by the migration domain; there is no old-to-new lineage conversion.

## Catalog update lifecycle

Canonical event append updates the actor's current summary and publishes committed mutation
notifications immediately. The server's live catalog therefore remains current independently of
on-disk cache persistence. Persistent summary-cache updates are handed to one session-manager-owned
coordinator that retains only the newest pending summary per session and flushes all pending
sessions after a fixed maximum delay. Sustained activity cannot extend that deadline indefinitely,
and queued memory is bounded by session count rather than event count.

Each flush acquires catalog coordination for one bounded operation, writes the drained summaries in
one transaction, and closes the database through Switchy's backend-neutral `Database` lifecycle.
Bcode does not inspect, truncate, or otherwise manage WAL sidecars. Delayed writers compare
`updated_at_ms` and cannot replace newer catalog rows. A failed pre-commit batch is merged back into
the latest-value queue without failing the canonical append. Explicit deletion removes pending
summary state and is serialized with catalog persistence so delayed work cannot resurrect a deleted
row. Graceful daemon shutdown requests a final bounded flush; abrupt shutdown remains safe because
canonical session databases remain authoritative.

Draft-session composer reads and writes are synchronous because their catalog rows are authoritative,
not derived summaries. They use the same bounded database lifecycle but never enter the asynchronous
summary queue.

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

## Explicit bounded bulk migration

Bulk canonical migration is a server-owned maintenance workflow layered on the existing per-session
migration service. `SessionBulkMigrationStart` selects stable bounded catalog pages and either
inventories compatibility without mutation or, after exact confirmation, starts or joins the
existing per-session migration operation. The aggregate coordinator never implements a second
historical decoder or target path: released-format planning, frozen decoding, backup creation,
normalization, transactional current-target migration, projection rebuild, strict validation, and
receipts remain owned by `bcode_session_migration` and the policy-free current migration target.

Mutation requires `migrate-supported-sessions`. Each eligible session acquires exclusive maintenance
ownership before backup or canonical changes. A live owner or maintenance lock is reported as a
bounded blocker while later sessions continue. Verified backups and migration receipts are durable
per-session evidence; current canonical classification makes explicit re-invocation idempotent.
Search providers are not consulted or invoked by canonical migration.

Historical usage migration preserves the facts actually recorded by the historical writer. When a
usage event has no trustworthy fixed estimate or exact request-attempt attribution, migration marks
its cost explicitly unavailable. It does not resolve the session's current model or consult current
catalog pricing: doing so would assign a later mutable price to an earlier request and silently
change history. Token usage remains available, while unavailable historical cost honestly records
that the original billing context cannot be reconstructed.

Aggregate operation IDs, revisions, cursors, progress, and outcome samples are bounded daemon-local
notification state. They define no retention, acknowledgment, replay, or conflict protocol and are
therefore neither reconnect-safe nor durably resumable. After daemon restart an old aggregate ID is
unavailable; the user explicitly invokes inventory or migration again. Durable current-session
classification and receipts determine which sessions still require work. Cancellation is
cooperative between per-session units and does not roll back a completed session migration.

## Current runtime and migration boundaries

`bcode_session` owns only the current writer contract, current event model, current projections,
and bounded current-session reads and writes. Historical payload DTOs, released writer-epoch
planning, historical classification, and conversion belong to the dedicated
`bcode_session_migration` domain crate. Historical DTOs must not leak into session models, client
protocols, or runtime APIs.

Transitional exception: `bcode_session` still contains historical compatibility and migration
implementation while extraction into `bcode_session_migration` is completed. This exception is
limited to existing migration paths; new historical policy or legacy-format handling must not be
added to `bcode_session` or `bcode_session_models`.

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
through that owner. Lease metadata includes writer epoch and daemon instance identity. Schema-v3
owner records are published by locking and synchronizing a unique temporary file before atomic
rename; the owning actor retains that locked file handle. Liveness for v3 comes from lock evidence,
not PID identity. Schema-v2 records retain conservative PID-based classification for compatibility,
and malformed or unknown owner metadata is unverifiable and never pruned as stale. A live older
daemon is never forced out or migrated underneath: it continues until release, idle close, graceful
stop, or process exit. A newer daemon waits or reports actionable owner metadata without taking the
database lock. Maintenance-to-runtime handoff must have no unowned writable gap, and an older writer
must not reacquire after epoch advancement.

## Automatic known-legacy migration

Normal first load is serialized per session. The manager inspects the migration ledger and durable
storage contract without mutation, then follows one of these paths:

* Current storage: acquire a compatible runtime lease and recheck compatibility while ownership is
  held. If its canonical envelope schemas are released historical inputs rather than current, route
  first open through the same exclusive migration operation before strict current decoding.
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
writer contract is epoch `7`; epoch `6` sessions migrate exclusively by rebuilding the new cumulative
usage projection alongside every existing required projection. Earlier recognized legacy epochs
follow the same complete migration chain. That rebuild applies
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

Schema-v3 owner publication uses `std::fs::File` locking: `flock` on Unix and `LockFileEx` on
Windows. The live guard retains the locked published file handle through its full lifetime. CI runs
the focused cross-process lease suite on Linux, macOS, and Windows so platform lock, rename, crash,
and stale-classification behavior cannot silently diverge.

A loaded actor owns the runtime lease rather than the manager registry. It releases database/event
caches and the lease atomically when its attached-client count and typed ownership-guard counts are
all zero. Queued commands, runtime work, and plugin invocations carry clone-safe typed guards through
their terminal persistence boundary; dropping the final guard triggers actor-side reevaluation.
Summary/catalog state and broadcast brokers remain available without retaining or reacquiring a
runtime lease. A later mutation or protected activity reacquires through the actor before opening a
runtime database or write lock. Explicit owner release uses this same serialized quiescence decision
and reports stable blocker categories without cancelling work or detaching clients.

## Ownership activity matrix

| Activity | Ownership carrier | Terminal boundary |
| --- | --- | --- |
| Attached client | Actor client registration | Actor detach or rolled-back/cancelled attach reply |
| Queued prompt, skill, or compaction | `QueuedCommand` guard in the queue item | Command terminal processing, send failure, queue drain, or runtime exit |
| Active turn and nested permission/exchange work | Queue guard retained by the runtime command | Turn terminal persistence and invocation-sink flush |
| Registered runtime/workflow work | `RuntimeWork` guard in the registry entry | Terminal durable runtime-work event and result persistence |
| Plugin invocation and input route | `PluginInvocation` guard in the active registration | Registration drop after trailing canonical events |
| Known-legacy migration | Exclusive maintenance coordinator, write guard, then handed-off runtime lease | Failed handoff drops maintenance; successful handoff is adopted and actor quiescence reevaluated |
| Bounded history/projection/composer/input reads | None | Read completion |
| Event/runtime-work subscriptions and attach forwarders | None beyond an actual attached client registration | Receiver/forwarder drop |
| Background invariant selection | Bounded task/catalog snapshot only | Selector task completion or stale-task cancellation |
| Cached session database connection | Actor-owned `SessionDb` handle, never handed out as a clone | Explicit backend close during quiescent release or idle timeout, verified terminal before release is reported |

No state envelope or snapshot in this matrix implies durable reconnect/resume. Durable resume would
require an explicit retention, acknowledgement, replay, and conflict contract.

### Release completeness

Releasing runtime ownership relinquishes the lease and the database connection together. Dropping a
cached handle is not sufficient: `SessionDb` wraps a shared connection, so a surviving clone would
keep the backend connection and its process-level file locks alive after the lease record was
already removed. That produces an orphaned lock with no owner record, which no ownership recovery
command can resolve because owner resolution starts from lease metadata.

Quiescent release therefore closes the connection through the backend lifecycle and probes it before
reporting success. When the connection cannot be proven terminal, release reports a blocked outcome
carrying `database_handle_retained` instead of claiming success, so a retained handle is surfaced
rather than concealed. That condition reaches clients as the typed
`SessionOwnershipBlocker::DatabaseHandleRetained` release blocker. Actor database accessors return
borrows rather than clones so the compiler keeps the actor the sole owner of the handle.

Read-only and diagnostic paths deliberately do not close. Closing checkpoints the WAL and rewrites
`session.db` and `session.db-wal`, which would make a normal read mutate canonical storage. Those
paths drop their local handle instead, which releases the connection without mutation.

`bcode session diagnose` does not dead-end when the canonical database is locked. It reports the
lock error together with lease observations and every verified live daemon candidate (namespace,
artifact, instance id, pid, build fingerprint, writer epoch, classification, and whether a lease
record names it), without opening or mutating canonical storage. Only daemons with verified identity
evidence are reported, so unverifiable ownership still fails closed.

## Historical daemon record classification

Daemon registry cleanup uses one conservative classification based on four independent facts:
current-build readiness, exact decoded endpoint identity, raw endpoint reachability, and process
identity from PID, process start time, executable path, and executable digest. Exact responsive
historical daemons remain controllable through their verified IPC endpoint. A historical process
whose protocol cannot be decoded is preserved when independent process identity is exact; Bcode
does not invoke its cached executable because an unknown historical CLI cannot be proven to stop
without spawning or replacing a daemon. Such records require an explicitly reviewed force action.
Responsive identity mismatches and unverifiable evidence are preserved and refused. Only an
unreachable record with positive missing/reused-process evidence is stale and removable. Cached
images referenced by every preserved record remain retained.

## Historical epoch-root recovery

An earlier, reverted implementation briefly wrote sessions beneath:

```text
<state-dir>/session-storage/writer-epoch-2/
```

Only `bcode_session_migration` may recognize this exact historical path or decide which entries are
recoverable. Current `bcode_session::migration_adapter` code supplies lease coordination and the
canonical atomic rename primitive without owning historical-layout policy. The path is migration
input, never an active session store.

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

Each projector advances its checkpoint only after its projection update succeeds. Within one append
transaction, all checkpointed projections are advanced together by a single multi-row upsert once
every projection update has succeeded; this preserves the same atomic outcome (no checkpoint is
visible without its projection, and any failure rolls back the complete append) while keeping the
number of statements per canonical append bounded. Each Turso statement is a full round trip, so the
append hot path treats statement count as a first-class cost. Missing, stale, incompatible, or
discontinuous required projections reject and roll back the append.

Turso opens a fresh connection per transaction, and on its default `temp_store` every
`INSERT ... RETURNING`/`UPSERT` result buffer is an ephemeral table backed by a new temporary
directory per statement. Bcode sets `PRAGMA temp_store = MEMORY` on every session and catalog
connection and at the start of every append/maintenance transaction. Temp objects never outlive a
statement, so this changes no durability property of canonical rows or the WAL; it removes a
`mkdir`/`open`/`fcntl`/`rmdir` cycle from each write statement. The release-mode
`benchmark_canonical_append_path` test (run explicitly with `--ignored`) is the reference
measurement for this path.

`manifest.json` is refreshed from the append path only when a display field changes or the
activity timestamp has drifted beyond a short interval; releasing the idle database handle writes
the final summary. Because the manifest is a disposable display cache and the catalog coordinator
coalesces independently, this cannot change canonical history or discovery correctness — a session
directory is discovered from `session.db` regardless of manifest freshness.

## Connection lifetime

Turso takes an exclusive `fcntl` lock on `session.db` for every non-read-only open, and Bcode runs
without Turso's experimental multi-process WAL. A held connection therefore excludes every other
process — other daemon artifact versions, doctor/repair, migration — until it is dropped. That is
why the invariant *released session ownership is completely released* is enforced by closing the
connection whenever ownership ends (quiescent release after a top-level mutation, idle timeout, or
shutdown), rather than by pooling connections across ownership boundaries.

Inside one ownership span the opposite rule applies: the session actor owns exactly one
`SessionDb` and every path reuses it. A persistent load opens the database once and threads that
handle through compatibility check, lease re-check, write-readiness validation, projected-state load,
and into the actor as its write handle. Summary refresh, write-readiness re-validation, bounded
history reads, and health probes for a loaded session all route through the actor's cached handle
instead of opening a second connection to the same file. Only an *unloaded* session may be probed
with a short-lived direct open. The `session.db.open_total` counter is the reference measurement:
`loading_and_probing_a_session_opens_its_database_once` asserts one open per cold load and zero for
subsequent health probes. Per-transaction `connect()` calls inside Turso create logical connections
on the already-open database (shared pager, WAL, and page cache) and cost single-digit microseconds;
they are not file opens and are not pooled.

Required projections include current session state, input history, transcript spans, tool runs,
artifact references, runtime work, cumulative request-deduplicated session usage and fixed
request-time cost, request-context occupancy, model context, and turn receipts. Normal reads never
silently rebuild them. Bounded session attach returns the compact usage summary independently of the
resident transcript window, so reconnecting or loading older transcript pages cannot change the
session-wide token or cost totals.

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
* `bcode session diagnose <session-id>` reports writer, migration source/target/steps, projection,
  canonical-tail, owner/waiting, retained-backup, and recovery-guidance state without mutation.
  Repeated diagnosis is byte-preserving for current, legacy, damaged, and future stores.
* `bcode session doctor [<session-id>] [--catalog|--scan]` diagnoses database and WAL state without
  mutation. It never migrates, repairs, or reindexes; follow its result with an explicit maintenance
  command only when needed.
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
p95, and 6,911 µs maximum. The enforced release gate is p95 at or below 25 ms. Migration release
gates are 1,000 / 30,000 / 240,000 ms total duration and 1 / 8 / 32 MiB database growth for the
100 / 5,000 / 50,000-event profiles, with final WAL required to be zero after checkpoint. Peak RSS
must remain effectively event-count-independent across those profiles; the recorded release
baseline range is 220.7–221.4 MB. These
ceilings deliberately provide headroom over the measured debug-build results while still catching
order-of-magnitude regressions:

```text
cargo test -p bcode_session benchmark_current_session_preparation_latency -- --ignored --nocapture
```

The current daemon also enforces a 25 ms p95 gate for preparation after runtime ownership has
already established the store as current. Across 30 runs per profile, the 100 / 5,000 / 50,000-event
stores measured 6 / 5 / 7 µs p95. The fast path is keyed by the live runtime lease itself rather
than an independent cache, so it cannot outlive ownership or bypass first-open writer/ledger
classification:

```text
cargo test -p bcode_session benchmark_current_session_preparation_is_event_count_independent -- --ignored --nocapture
```

The table's peak RSS is `/usr/bin/time -l` maximum resident set size for the complete isolated test
process, so it is a conservative process high-water mark rather than migration-only allocation.
Across these profiles it remained effectively flat (220.7–221.4 MB). DB growth and peak RSS are
now measured, and explicit post-commit checkpointing leaves the final WAL at zero bytes for every
profile. Switchy's Turso `Database::exec_raw` rejects row-producing pragmas with `unexpected row
during execution`; `Database::query_raw("PRAGMA wal_checkpoint(TRUNCATE)")` is therefore the
supported abstraction and is timed separately.

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
* **Locked with no reported owner:** A database lock can outlive its lease record, in which case
  owner resolution reports no verified owner even though the database is locked. Run
  `bcode session diagnose <session-id>` to identify the holding daemon, then use
  `release-owner`/`stop-owner` against that daemon. Because Bcode intentionally supports many
  concurrent build-specific daemons, never stop a daemon that is still serving other clients; a
  daemon legitimately serving other sessions is not a stale owner.
* **Backup failure:** Migration does not begin mutation until backup verification succeeds. Check
  the reported filesystem error, free space, destination permissions, and destination conflicts,
  then retry normal open. Keep any retained backup path reported by Bcode. Do not replace the
  session database with a partial backup.
* **Unsupported future writer:** The store was last written by a newer Bcode build. Do not run
  repair, reindex, or migration with this older build; use a build that supports the reported
  writer epoch.
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
remove its retained backup path. `WaitingForOwnership` is reserved for non-terminal acquisition
progress. A concrete live-owner refusal terminates as `Failed` with `OwnedByOtherDaemon`; after the
owner releases, retry starts a fresh operation rather than rewriting the terminal result.

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
