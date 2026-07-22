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

Health, doctor, diagnosis, catalog, audit, and normal session-manager load paths remain
non-migrating. Legacy migration is available only through an explicit maintenance command.

## Explicit legacy migration

Normal first load is serialized per session. The manager inspects the migration ledger and durable
storage contract without mutation, then follows one of these paths:

* Current storage: acquire a compatible runtime lease and recheck compatibility while ownership is
  held.
* Known legacy storage: return migration-required without acquiring maintenance ownership or
  changing the database.
* Unknown migration ids, dirty or failed migration records, future writer epochs, unsupported
  contract schemas, malformed canonical history, or ledger/contract inconsistencies: fail closed
  without mutation.

Explicit migration acquires exclusive maintenance ownership and the maintenance write lock before
calling `migrate_turso_in_root`. Migration strictly validates contiguous canonical sequences and
session identity, preserves canonical events and drafts, rebuilds all required derived projections
through the same projector functions used by normal append, verifies checkpoints at the canonical
tail, and updates the writer contract only when validation succeeds.

## Durable writer contract

`session_storage_contract` contains a singleton versioned writer epoch. Mutation-capable processes
advertise their epoch in session leases and validate the durable row before mutation. The current
contract-aware baseline is epoch `2`.

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

* `bcode session diagnose <session-id>` reports writer, projection, canonical-tail, and ownership
  state without mutation.
* `bcode session doctor` diagnoses database and WAL state without mutation.
* `bcode session repair` acquires exclusive maintenance ownership, creates backups, and performs
  only supported database-sidecar/tail repair.
* `bcode session reindex` acquires maintenance and write capabilities before rebuilding projections.

Repair uses Bcode's native Turso stack. Stock SQLite checkpoint/repair is not the primary recovery
path. Catalog listing, picker display, normal attach, and paged history must never invoke repair.

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
* Known legacy migration requires explicit maintenance ownership and never runs on normal paths.
* Unknown, future, dirty, ambiguous, or corrupt storage fails closed.
* Historical duplicate roots are never merged automatically.
