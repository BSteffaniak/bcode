# Historical session migration fixtures

These fixtures are sanitized compatibility inputs. They must never contain private session text,
paths, credentials, or identifiers copied from a user store.

## Confirmed schema-28 incident family

Read-only inspection of the retained epoch-2 backup for incident session
`d4bc4495-89f6-4bcd-ac64-1b58d5642992` established this historical event inventory:

* 31 `tool_call_finished` events
* 22 `tool_invocation_stream` events
* 14 `context_usage_observed` events

`stores/schema-28-tool-context.jsonl` is a minimal contiguous synthetic reproduction containing one
representative event from each affected family plus the current semantic context needed to project
the tool invocation. It intentionally preserves no private incident payloads.

Expected migration treatment:

* `tool_call_finished` converts to `ToolInvocationResultRecorded`.
* `tool_invocation_stream` becomes current inert `OpaqueEvent` history and must not revive runtime
  streaming behavior.
* flat `context_usage_observed` converts to `RequestContextObserved`.

## Confirmed current-equivalent event families

`stores/released-current-equivalent-events.jsonl` contains one complete, sanitized payload for every
released historical event family whose active semantics are preserved by strict current decoding.
The fixture uses each family's earliest released schema. The fixture gate strictly deserializes
every row as a current `SessionEvent`, verifies the event kind is unchanged, and rejects malformed
stand-ins as coverage. Together with the explicit-conversion and retired-family fixtures, this gives
every released event family a permanent valid payload rather than relying on generated `{}` probes.

## Confirmed retired event families

`stores/retired-events.jsonl` contains synthetic classification coverage for every released event
family removed from the active runtime. Each representative row uses a schema that actually
released that family; the manifest additionally declares every released schema in that family's
exact inventory range, and the fixture gate replays each declared schema/kind pair by changing only
the stable envelope schema. It is not a complete store fixture and therefore claims no writer,
ledger, root, table, or writable-lifecycle coverage. Migration classification must preserve each
payload as inert current `OpaqueEvent` history without reviving obsolete interactive-tool,
plugin-automation, presentation, turn, or stream behavior.

## Released explicit-conversion boundaries

`stores/released-explicit-conversions.jsonl` contains sanitized classification fixtures for the
first and last released flat tool-result shapes (schemas 1 and 39), the early flat context-usage
shape (schema 26), and both invocation-wrapped context-usage schemas (30 and 31). The manifest also
claims every intervening released schema for those exact unchanged event families; the fixture gate
replays each declared schema/kind pair by substituting only the stable envelope schema and requiring
successful migration decoding. These rows pin the exact historical DTO boundaries without claiming
complete-store writer, ledger, table, root, or writable-lifecycle coverage. Store-level writable
lifecycle coverage for context schemas 26, 30, and 31 lives in the session migration tests, where
each shape rebuilds current occupancy and accepts a subsequent append.

## Released current-equivalent schema coverage

`stores/released-writer-schema-matrix.jsonl` contains one contiguous sanitized canonical history
with at least one strict-current-compatible event at every released historical event schema. Its
manifest classification claims prove every released schema can traverse the strict
current-equivalent path without claiming one specific writer epoch, ledger prefix, or table/root
treatment. The session migration lifecycle test materializes this same permanent history under
every released writer epoch, migrates it to current storage, strictly reads it, appends, reopens,
appends again, and validates a second reopen. Thus writer/schema cross-product and writable adoption
are proven at store level without duplicating 144 identical fixture files or making overlapping
exact-coverage claims in the manifest.

The migration crate additionally owns deterministic ledger fixture cases for every released
per-session migration identity. Current-materialized identities use exact ordered prefixes ending at
the declared endpoint. The two superseded identities use standalone historical cases, preventing
tests from pretending they were members of the current schema ledger.

## Manifest

`manifest.json` is the machine-enforced inventory for permanent sanitized fixtures. Every listed
path must exist, contain exactly the declared contiguous event count and schemas, cover exactly the
declared schema/kind pairs, and produce the declared migration classifications. Matrix-owner
fixtures additionally declare the released writer epochs under which their permanent payloads are
materialized by lifecycle tests; ownership is duplicate-rejecting across writer/schema/event
combinations, with explicit exclusions only for setup rows owned by another fixture. Classification-
only fixtures cannot claim ledger, root, table, or authoritative coverage. Fixture files not listed
in the manifest fail the inventory test.

The policy-free `bcode_session_migration_target` package owns the exact nine-operation capability
inventory plus the shared current event-schema and writer contracts. Both the current session
implementation and historical migration crate depend on this package. Historical source policy,
format inventory, and migration orchestration are forbidden from entering it.

A complete migration must normalize all canonical payloads to the current event schema, rebuild
current projections, produce zero compatibility issues, pass write readiness, and permit a new
append.
