# Historical session migration fixtures

These fixtures are sanitized compatibility inputs. They must never contain private session text,
paths, credentials, or identifiers copied from a user store.

## Reconstructed released writer-23/schema-55 compatibility inputs

`stores/epoch-23-schema-55-events.jsonl` reconstructs the released schema-55 event
representation from the sanitized schema-54 family matrix. Only the outer schema changes:
no model selection, effective-model evidence, request correlation, native turn identity,
or host inventory commitment is synthesized. It is a compatibility matrix, not a captured
writer-23 database or evidence of a successful database migration. Its lifecycle matrix
requests the existing store harness exercise all released writer sources through 23;
those claims remain subject to the concrete session-domain DB lifecycle tests.

Schema 56 and writer 24 are reserved, unsupported inputs. The selected-model transition
is the explicit writer 23 to 25 edge, targeting schema 57 only after version publication.
The earlier monotonic edges remain intact. Disabled recovery and interaction families
remain unsupported even when the target schema is 57.

## Reconstructed released writer-25/schema-57 compatibility inputs

`stores/epoch-25-schema-57-events.jsonl` reconstructs the released schema-57 event
representation from the sanitized schema-55 family matrix by changing only the outer schema.
Like the schema-55 matrix it synthesizes no selected-model evidence; selected-open operations
remain covered by the session-domain selected-model migration tests. Writer 25 migrates to
writer 26 through the explicit `session-writer-epoch-25-to-26` edge, which preserves canonical
payloads and advances only the event schema (57 to 58) and writer contract.

Schema 58 / writer 26 fences managed native-parity records: reasoning, native tool-result and
native usage observations, text segment and tool input display metadata, interaction tool
metadata, and configuration requests. Historical inputs at schema 57 or earlier that carry any
of these are rejected rather than guessed. Recovery records remain unpublished at schema 58.

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

## Epoch-5 development schema 40

`stores/epoch-5-schema-40-events.jsonl` captures the sanitized schema-40 event surface emitted by an
epoch-5 development build that wrote durable user sessions. Although that format was not a release
boundary, those sessions are real migration inputs. Strict current reads continue to reject schema
40; exclusive epoch-5-to-6 migration normalizes it to the current schema and rebuilds projections.

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

## Epoch-16 operational evidence

`stores/epoch-16-schema-52-events.jsonl` preserves schema-52 canonical session creation and
turn-independent provider evidence. Epoch 16 and epoch 17 share event schema 52; the 16-to-17
migration rebuilds the derived evidence projection and advances only the writer contract.
The fixture is registered for epoch-16 lifecycle input without claiming overlapping writer/schema
or writer/schema/event ownership. Existing matrix-owner fixtures also include epoch 16, covering
the historical schema/event cross-product with the same duplicate-rejecting ownership rules.
Replay tests verify accepted and retired synthesis envelopes, preserve unsupported envelope data,
and reject unsupported versions rather than downgrading them. Finalization tests ensure interrupted
or stale evidence rebuilds cannot advance the writer contract and that interruption permits retry.

## Managed external evidence projection upgrade

Writer epoch 18 adds reserved external evidence locator kinds in the existing `turn_evidence`
projection. Canonical event schema advances from 52 to 53. The 17-to-18 edge preserves canonical events and
rebuilds derived locators through the existing exclusive, interruption-safe migration path; an
older writer cannot safely maintain these locators and must remain fenced. Current-format external
evidence uses explicit semantic `SessionEventKind` variants, rejects unsupported versions, and retains typed normalized
bindings, accepted submissions, delivery receipts, observations and interaction responses. There is
no native protocol recovery journal or implicit mutation replay.

`stores/epoch-17-schema-52-events.jsonl` adds schema-52 classification coverage for existing
semantic variants, including submission recovery/cancellation. It claims schema/event matrix
coverage only, not complete writable-store lifecycle behavior or ledger/table authority.
The earlier epoch-16 evidence fixture now expects historical conversion rather than current-schema
passthrough. Diagnostic trace contents remain opaque and never become external semantic events.

Fixture lifecycle matrices now include epoch 17. Migration tests cover lossless normalization of
external evidence and forward-only 17-to-18 planning. Session-owned offline behavioral tests cover
bounded indexed reads, correlation, deduplication, generation fencing and terminal evidence. Neither
historical migration nor reconstruction of a locator authorizes native execution or ownership transfer.

## Writer-21/22 compatibility and evidence scope

Writers 21 and 22 remain accepted historical source contracts. No authentic producer snapshot or
source-store fixture is available for those epochs. Their absence is a provenance limitation, not
proof that those writers never ran and not a reason to withdraw compatibility.

The fixture gate counts every edge in a fixture's complete planned upgrade path, including
20→21→22→23. Source-writer coverage remains separate: traversing these edges does not claim that
writer 21 or 22 emitted a fixture. No historical producer attestation is required for these two
epochs. Reconstructed inputs must be labeled as contract tests, never authentic historical exports.

Migration-owned normalization tests exercise exact writer-21/22 commitment preservation, malformed
commitment rejection, writer-22 managed-tool claim metadata preservation and its rejection for older
writers. Isolated real-database contract tests exercise reconstructed writer-21/22 stores through
ordinary-read refusal, production migration, strict current readiness, receipt validation, indexed
evidence checks and repeated reopen. Existing historical-store tests exercise complete upgrade paths.
These tests validate the represented contracts, not the behavior of an unavailable historical binary.

The migration planner, source admission, canonical transformations and version fences are unchanged.
Writer-21/22 commitments must not be cleared or regenerated. Claim metadata attributed to writers
at or below 21 remains rejected. The current target populates canonical event-row identity keys and
independently validates payload-derived keys and claim evidence before writer publication. Unknown
versions and unsupported stores continue to fail closed without guessing their representation.

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

Schema 52 / writer epoch 16 is retained in `stores/epoch-16-schema-52-events.jsonl`.
Its lifecycle matrix covers every supported historical writer epoch. Schema-52 migration
preserves recorded invocation, exchange, submission, usage and child identities; it does not
reconstruct producer state from exchange requests or create durable interaction checkpoints.
`durable_interaction_committed` starts at schema 53 and is rejected in schema-52 input.
The 16-to-17 writer transition uses the existing exclusive migration, strict validation and
final writer-contract advancement path; an unchanged SQL ledger does not bypass writer fencing.

A complete migration must normalize all canonical payloads to the current event schema, rebuild
current projections, produce zero compatibility issues, pass write readiness, and permit a new
append.
