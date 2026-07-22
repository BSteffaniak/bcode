# SDK session persistence adapters

`SessionPersistenceAdapter` is the portable synchronous persistence boundary for embedded SDK
sessions. It does not depend on Bcode's local session database, catalog, daemon, or repair paths.
Applications may store `PersistedSession` in files, databases, object stores, browser/desktop
bridges, or their own services.

## Versioned payload

`PersistedSession.schema_version` uses `PERSISTED_SESSION_SCHEMA_VERSION`. Version 1 stores the
session ID, visible `ModelMessage` transcript, and explicitly persisted `MemoryItem` metadata.
Unversioned JSON from the initial SDK format decodes as version 1 with no memory records.

Bcode validates the payload returned by every adapter. Unsupported versions fail with a migration
message before session state is used. Persisted memory records must have `SessionTranscript`
retention and a corresponding visible transcript message. Bcode does not mutate, repair, downgrade,
or silently discard damaged adapter data.

When the schema changes incompatibly, adapters should decode their previous representation, migrate
it to the current complete `PersistedSession`, atomically save that representation under their own
migration policy, and only then return it. Applications should retain rollback/backups according to
their storage requirements.

## Atomicity and concurrency

A successful `save` means the complete payload is visible to a later `load`; partial records must
never be exposed. Adapters must serialize concurrent writes to one logical session and define their
own conflict policy (for example optimistic versioning, a transaction, or last-writer-wins). Errors
must leave the previously committed payload readable.

The bundled `LocalSessionStore` writes JSON to a same-directory temporary path and renames it over
the destination. Callers must serialize writers to the same path; the last completed save wins. It
is a convenience for one application process, not a multi-process transactional database. Its
session object is committed only after persistence succeeds.

## Error and durability expectations

`load` returns `Ok(None)` only for absence. Corrupt, unsupported, stale, inconsistent, or conflicting
state returns an actionable `BcodeError::SessionState`/`SessionPersistence` rather than an empty
session. Adapters choose their durability level (memory, process, filesystem, database transaction,
remote acknowledgment) and should document it for their application. Bcode invokes persistence only
when explicitly configured and after successful conversation turns; stateless generation remains
independent.
