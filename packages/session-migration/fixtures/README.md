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

## Manifest

`manifest.json` is the machine-enforced inventory for permanent sanitized fixtures. Every listed
path must exist, contain exactly the declared contiguous event count and schemas, cover exactly the
declared event kinds, and produce the declared migration classifications. Fixture files not listed
in the manifest fail the inventory test.

A complete migration must normalize all canonical payloads to the current event schema, rebuild
current projections, produce zero compatibility issues, pass write readiness, and permit a new
append.
