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

## Confirmed retired event families

`stores/retired-events.jsonl` contains synthetic classification coverage for every released event
family removed from the active runtime. Each row uses a schema that actually released that family;
it is not a complete store fixture and therefore claims no writer, ledger, root, table, or
writable-lifecycle coverage. Migration classification must preserve each payload as inert current
`OpaqueEvent` history without reviving obsolete interactive-tool, plugin-automation,
presentation, turn, or stream behavior.

## Released explicit-conversion boundaries

`stores/released-explicit-conversions.jsonl` contains sanitized classification fixtures for the
first and last released flat tool-result shapes (schemas 1 and 39), the early flat context-usage
shape (schema 26), and both invocation-wrapped context-usage schemas (30 and 31). These rows pin the
exact historical DTO boundaries without claiming complete-store writer, ledger, table, root, or
writable-lifecycle coverage. Store-level writable lifecycle coverage for context schemas 26, 30,
and 31 lives in the session migration tests, where each shape rebuilds current occupancy and accepts
a subsequent append.

## Manifest

`manifest.json` is the machine-enforced inventory for permanent sanitized fixtures. Every listed
path must exist, contain exactly the declared contiguous event count and schemas, cover exactly the
declared writer/schema and schema/kind pairs, and produce the declared migration classifications.
Classification-only fixtures cannot claim store-level writer, ledger, root, table, authoritative,
or writable-lifecycle coverage. Fixture files not listed in the manifest fail the inventory test.

A complete migration must normalize all canonical payloads to the current event schema, rebuild
current projections, produce zero compatibility issues, pass write readiness, and permit a new
append.
