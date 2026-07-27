# Workflow Persistence Architecture

## Canonical ownership

Durable workflow execution uses one dedicated database:

```text
<state-dir>/workflows/workflow.db
```

`bcode_config::default_state_dir()` owns resolution of `<state-dir>`. The workflow persistence
package owns the `workflows/` directory and database path. Session, plugin, loop, and TUI code must
not construct alternate workflow roots or maintain competing workflow state files.

The workflow database is authoritative for normalized definitions, runs, activations, attempts,
validated outputs, artifact references, decisions, grants, resource leases, dispatch receipts,
and workflow event/projection checkpoints. Canonical session transcript databases remain
independent and contain only compact generic relationships or user-facing status events where a
real integration requires them; detailed workflow rows never belong in session history.

## Durable production admission

The durable daemon binds registration and start validation to
`WorkflowProductionCapabilities::current()`. That versioned capability contract covers the
compiled definition and predicate versions, transform/retry-policy availability, agent
configuration version, workflow-block interface version, node support classifications, parallel
join policies, artifact references, and agent execution targets. It is deliberately distinct from
the broader in-process SDK capability surface.

Current durable support is:

* supported: `Agent`, `Branch`, `Repeat`, wait-all `Parallel`, `PluginBlock`, `Input`, and
  `Approval`;
* in-process-only: closure-backed `Task`;
* rejected pending complete durable behavior: `Retry`, retry edges, `FanOut`, fail-fast parallel,
  and declarative transforms.

Registration rejects unsupported definitions before persistence and resolves every plugin block to
an exact enabled manifest declaration. Start repeats the same admission and resolution so a
previously registered definition fails closed when its owner plugin is disabled or incompatible.
Unsupported definitions and unavailable capabilities use the stable IPC error codes
`workflow_definition_unsupported` and `workflow_capability_unavailable`.

## Initial normalized schema

Migrations are added only with behavior that reads and writes their tables. The first durable
slice requires these identities and relationships:

* `definitions`: definition id, version, canonical serialized definition, and checksum.
* `runs`: run id, definition identity, immutable workspace snapshot, parent session, status,
  creation/update timestamps, cancellation intent, and limits.
* `activations`: run/node/activation identity, dependency generation, status, and validated output
  reference.
* `attempts`: run/node/activation/attempt identity, prepared dispatch intent, side-effect class,
  status, admission/service receipt, timestamps, and ambiguity/repair state.
* `outputs`: schema identity/version, validated bounded inline value or artifact reference, and
  checksum.
* `decisions` and `grants`: bounded policy decisions and non-secret grant identities/scopes.
* `resource_leases`: normalized run/node resource ownership with lease generation.
* `workflow_events`: bounded append-only operational history for paged inspection.
* `projection_checkpoints`: projection name/version and last applied event sequence.

Stable dispatch identity is derived from `(run_id, node_id, activation_id, attempt)` and persisted
with prepared intent before an external operation is invoked.

## Explicit retained state

Durable definitions carry retained context with the version 1
`WorkflowStateEnvelope<State, Value>` schema. `state` is the explicitly forwarded original/evolving
workflow state, `value` is the narrow request or result for the current node, and `artifacts` holds
typed `ArtifactReference` values for large data that must not be copied inline. The envelope is
ordinary serialized node data: it participates in schemas, transforms, checksums, outputs, and
history. Hosts must not maintain a second hidden mutable workflow-context object.

## Target-input validation

Once durable support is enabled, activation input is validated against the exact target node schema
at every insertion boundary. This includes run entry materialization, direct and conditional
successors, parallel join values, repeat back-edges, public activation insertion, and waiting-gate
successors. Validation occurs in the same transaction as output completion and successor
materialization, so a mismatch leaves the source activation/output and target activation unchanged.
The returned typed diagnostic identifies the run, source node and activation, target node and schema,
and the exact validator failure so callers can surface or persist an actionable failure without
inferring context from an unstructured message.

Automatic retry eligibility is a versioned owner-neutral policy over persisted facts: node effect,
owner reconciliation contract, stable failure kind, completed attempt count, definition maximum, and
run retry cap. Cancellation, terminal timeout, approval denial, schema failure, ambiguous mutation,
and terminal failure are never eligible. A mutating owner-reported failure requires receipt/status
reconciliation; repair-required mutation is never automatically retried. This pure decision does not
schedule work. The workflow store schema version 6 persists one
exact retry schedule per activation with failed/next attempt numbers, failure kind, backoff duration,
due timestamp, and scheduling timestamp. Scheduling is idempotent and never sleeps or creates the
attempt; conflicting reschedules fail closed. Production automatic retry remains unsupported until a
bounded scheduler safely consumes due schedules.

Repeat iteration is the persisted activation `dependency_generation`, starting at zero. Settlement
computes the next generation with checked arithmetic and applies the effective bound
`min(definition.max_iterations, run.cycle_cap)`. The settlement event records current/next
generation plus both configured bounds. If the predicate clears at the final allowed generation the
run completes; if it remains true, the run fails with `repeat_iteration_limit_exhausted`. Back-edge
input is transformed and schema-validated before the next-generation activation is inserted. Stable
activation identity plus transactional settlement makes reopening idempotent: the pending controller
can create only its exact next generation, and subsequent settlement cannot duplicate or skip it.

Canonical branch decisions include the predicate contract version, the selected boolean, selected
entry IDs, and skipped node IDs. The decision row is inserted in the same transaction before any
selected successor activation. A failure after that insertion rolls back the decision, skipped
markers, output, and activation together, allowing restart to recompute the same decision from the
persisted definition and source input.

Canonical fan-out results use version 1 `{ index, value }` members in strict contiguous ascending
input-index order. This shape is independent of completion order and rejects sparse or reordered
members. The in-process SDK implementation enforces bounded concurrency and preserves this ordering,
but production `FanOut` remains rejected until durable item admission, resource/cancellation state,
and restart-safe partial completion are implemented.

Canonical parallel joins declare non-empty, disjoint `left_exits` and `right_exits` sets whose
members have direct edges to the join. Durable materialization always serializes the tuple as
`[left, right]`, independent of branch completion order. A join-edge transform can address those
same persisted values through the stable `join.left` and `join.right` source names. Other transforms
can address the selecting node output as `current` and immutable run input as `state`.

Canonical state transitions and required projections commit atomically. In particular:

1. Persist prepared external-operation intent before dispatch.
2. Dispatch with the persisted stable identity.
3. Persist the returned admission/service receipt before reporting the attempt as admitted.
4. Observe completion through bounded durable status/event APIs.
5. Validate and persist output before making downstream activations ready.
6. Persist cancellation intent before signaling active children.

A process crash may leave an attempt prepared, admitted, or running. Restart reconciliation must
use the persisted identity and receipt. It must never blindly duplicate an operation whose
mutating outcome is unknown.

## Bounded normal reads

List, status, open, and attach paths read bounded run/projection rows and paged workflow events.
They must not replay the complete workflow event history, scan every attempt, contact external
systems, or run repair. Summaries are projection-backed and include an explicit stale/degraded or
repair-required state when trust cannot be established.

## Reconciliation and repair

Automatic reconciliation is allowed only when durable receipts and owner APIs prove the current
operation state. Prepared mutation without a trustworthy receipt or externally provable outcome
becomes `repair_required`; it is not retried automatically.

Full replay, projection rebuild, receipt investigation, forced retry, and ambiguity resolution are
explicit doctor/reconcile/repair operations. Maintenance acquires exclusive workflow-store
ownership and records its outcome. Normal read paths remain non-mutating even when the database is
damaged or stale.

## Migrations and compatibility

The workflow database has its own migration ledger and storage contract. Migrations are ordered,
idempotent where practical, and never selected by build namespace. A newer incompatible schema or
unknown migration fails closed with an upgrade/repair diagnostic. Destructive rebuilds require an
explicit maintenance command and verified backup once user-created durable runs exist.

## Architecture enforcement

Once the durable package exists, `scripts/check-workflow-architecture.sh` must enforce at least:

* only the workflow persistence owner constructs `workflow.db`;
* session and loop packages do not define workflow tables or state files;
* normal workflow list/status paths do not call replay, repair, or external dispatch APIs;
* prepared intent precedes dispatch and validated output precedes downstream activation;
* ambiguous mutating attempts transition to repair-required rather than automatic retry.
