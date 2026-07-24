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

## Transaction boundaries

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
