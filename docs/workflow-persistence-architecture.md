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
and workflow event/projection checkpoints. It is also the sole canonical store for runtime-authored
logical workflows, mutable drafts and their generations, immutable published revisions, active
revision pointers, revision-bound presets, authoring/publication events, and exact links from authored
revisions to compiled definitions. These records extend the existing database; authoring clients,
plugins, sessions, and renderers must not create another workflow catalog or draft store. Canonical
session transcript databases remain independent and contain only compact generic relationships or
user-facing status events where a real integration requires them; detailed workflow rows never belong
in session history.

Workflow prompt recovery uses a versioned `workflow_execution_sessions` relation keyed by exact run,
node, activation, and attempt identity. The relation stores only the opaque session ID, immutable
workspace snapshot, and creation time; session content, visibility, retention, and history remain
session-owned. Identical insertion is idempotent, while activation/session identity reuse or damaged
cross-domain provenance fails closed. A bounded recovery lookup may find one unique execution session
from the current canonical provenance contract and persist a missing relation; this repairs only a
derived link, never decodes an obsolete workflow schema. Ambiguity is surfaced rather than silently
selected.

Workflow execution sessions are durable background sessions: normal pickers exclude them, while
direct inspection and explicitly background-inclusive bounded catalog APIs expose them. Their
workflow-store links are retained with the owning run for restart and audit; ordinary run completion
or cancellation does not delete session history or unlink provenance. Cleanup follows explicit
session/run retention or deletion policy rather than renderer lifetime, daemon restart, or catalog
visibility. Shared-parent agents create no child session and are serialized by a parent-session lock
held through turn completion; fixed/fresh children remain activation-scoped.

Runs containing fixed-generation agents must pin one exact parent-session generation during
start admission. The daemon verifies that generation before creating canonical run state, persists it
on the run, and each activation derives only parent events through the pinned generation using the
session layer's bounded generic derivation engine. Later
workflow status or user events cannot enter that child context, while missing, future, stale-at-start,
or cross-session generation facts fail closed.

## Runtime-authored workflow lifecycle

The authoritative source/compile/run boundary is defined in
[`runtime-workflow-authoring.md`](runtime-workflow-authoring.md). A mutable draft uses optimistic
concurrency and is never executable authority. Publishing atomically records an immutable authored
revision, its exact compiled `WorkflowDefinition` link, a publication event, and an optional
compare-and-set active-revision update. A failed publish leaves none of those records partially
visible.

Published revision rows are append-only. Activating another revision changes only the logical
workflow's convenience pointer. Every authored run records the exact logical workflow revision and
compiled definition selected at admission, so later draft edits, publication, activation, archive, or
preset changes cannot rewrite an active or historical run.

Presets are mutable generation-checked configuration records bound to one exact published revision.
They are not part of revision identity and do not carry grants or inline secrets. Starting from a
preset persists the exact preset generation, resolved revision, validated final configuration, and
compiled definition used by the run.

Normal workflow, draft, revision, preset, validation, preview, and status reads are bounded and
non-mutating. Validation reports, current requirement availability, and catalog projections are
derived rather than canonical. Missing definition links, invalid active pointers, future versions, or
inconsistent revision relationships surface degraded or repair-required state; normal reads do not
reconstruct or repair them.

## Durable production admission

The durable daemon binds registration and start validation to
`WorkflowProductionCapabilities::current()`. That versioned capability contract covers the
compiled definition and predicate versions, transform/retry-policy availability, agent
configuration version, workflow-block interface version, node support classifications, parallel
join policies, artifact references, and agent execution targets. It is deliberately distinct from
the broader in-process SDK capability surface.

Current durable support is:

* supported: `Agent`, `Branch`, `Repeat`, wait-all/fail-fast `Parallel`, `PluginBlock`, `Input`, and
  `Approval`;
* in-process-only: closure-backed `Task`;
* rejected pending complete durable behavior: `Retry`, retry edges, and `FanOut`.

Versioned bounded declarative edge transforms are supported for direct, conditional, repeat, and
canonical parallel join materialization.

Registration rejects unsupported definitions before persistence and resolves every plugin block to
an exact enabled manifest declaration. Start repeats the same admission and resolution so a
previously registered definition fails closed when its owner plugin is disabled or incompatible.
Unsupported definitions and unavailable capabilities use the stable IPC error codes
`workflow_definition_unsupported` and `workflow_capability_unavailable`.

Versioned durable prompt configuration is serialized into definition identity and covers execution
target, profile, provider/model, structured output, read-only/tool policy, allowlist, timeout, and
prompt text. Workflow contracts contain no skill IDs, activation modes, requirements, or model-policy
resolution. Prompt text may request skills through the ordinary agent skill catalog and tool path,
but skill availability is not an admission requirement and skill metadata cannot widen the configured
tool or authorization ceiling. Read-only workflow prompts require read-only tool capability.

## Plugin-owned blocks and templates

Workflow blocks, template declarations, typed transform/state-envelope guidance, mutation approval,
and repair-required behavior are documented in
[`workflow-plugins-and-templates.md`](workflow-plugins-and-templates.md). Template discovery is
manifest-driven and non-executing; template start revalidates requirements and configuration before
persisting the exact compiled definition.

## Durable mutation approval requests

Mutating plugin-block approval uses `WorkflowMutationGrantScope` version 1. Its immutable identity
binds definition/version, run, node, activation, workspace snapshot, plugin/block/version/operation,
mutating capability, and the SHA-256 checksum of the canonical activation input. The workflow store
persists this request and changes the activation from `pending` to `waiting_mutation_approval` in
one transaction. No attempt is prepared and no owner is called first. Equivalent duplicate requests
are idempotent; stale input, workspace, or activation identity fails closed. Pending requests are
bounded and survive restart.

Resolution is also one transaction. Approval writes the immutable decision and exact grant before
changing the activation back to `pending`; denial writes the decision and fails the activation/run
without dispatch. Expired requests fail closed without a grant. Equivalent duplicate approvals
return the existing grant, while conflicting later decisions fail. Bounded indexed request/grant
queries expose identity and status without activation input. Approved pending work and its single
exact grant survive restart with no attempt created before scheduler admission. Portable IPC and
client APIs list bounded pending approvals and resolve exact approval IDs with typed approve/deny
decisions and typed resolution results; the server delegates those operations to the same atomic
store boundary. Run cancellation changes pending approvals to `cancelled` in the cancellation-intent
transaction before normal run finalization, creating no grant or attempt and rejecting later
approval decisions.

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

## Canonical terminal output and child composition

Public run inspection includes the bounded canonical terminal value alongside its exact output
identity, schema identity, checksum, artifact reference, and timestamp. This is a normal bounded
store lookup; it does not replay workflow history or load referenced artifacts. Child callers still
consume only this same canonical output. The exact successful exit activation commits that output
atomically with the terminal run transition.
Byte-equivalent duplicate settlement is idempotent; conflicting outputs, ambiguous successful exits,
or stale updates fail closed. Failed, cancelled, paused, and repair-required runs never expose a
successful terminal output.

Synchronous child-workflow calls add canonical parent/child links to this database. A link records the
root run, parent run and activation/attempt, deterministic child run identity, exact immutable target,
workspace identity, and lifecycle timestamps. The link and child creation commit before parent
dispatch admission is acknowledged. Restart reconciliation follows the persisted child identity and
receipt rather than creating another child. Parent cancellation propagates to the child and waits for
a stable outcome; version 1 does not abandon children.

Child dependency depth, descendant count, recursion, exact target, and output-schema compatibility
are validated before start. Child duration, node-execution, concurrency, cycle, and retry limits can
only narrow the inherited parent envelope; durable admission rejects widening and accounts root-tree
attempts before adding another descendant. The scheduler settles an elapsed idle-run deadline once
without replay; active external attempts continue through normal cancellation and authoritative owner
observation. Parent resource leases are not retained while waiting for a child that may need them.
Child grants do not become ambient parent authority, and parent grants apply to descendants only
through an explicit exact descendant-operation scope.

The complete contracts and fixed bounds are defined in
[`composable-coding-workflows.md`](composable-coding-workflows.md).

## Atomic package lifecycle

Workflow package manifests are bounded portable inputs. Clients confine local member paths before
transport; portable validation independently bounds members, source bytes, direct and total edges,
dependency depth, exports, external dependencies, versions, duplicates, and cycles. Pure planning
lowers members child-before-parent and retains package-qualified source maps. Preview diagnostics are
remapped through those maps without persistence.

Apply and publish use exact typed plans, locks, and optimistic member generations. Existing members
require exact generations; omitted members are create-only. Every member is staged in one immediate
database transaction, so conflicts or injected failures expose no partial draft, revision, or lock
facts. Publication regenerates the authoritative lock only after all canonical revisions commit.
Validate, preview, apply, and publish are available through IPC, client, CLI, and workflow-plugin
application boundaries; frontends consume the renderer-neutral typed results.

## Durable agent configuration

The current `WorkflowPromptConfiguration` is the strict serialized prompt-node contract. It includes
the execution target, profile, provider/model overrides, strict structured-output schema, read-only
and tool-capability policy, tool allowlist, timeout, and prompt mode/system prompt. Unknown fields,
unsupported versions, duplicate tool IDs, invalid schemas, and read-only mutation escalation fail
admission. Skill requests belong in prompt text and use ordinary agent infrastructure rather than
durable workflow fields.

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
run retry cap. Plugin-owned blocks may declare the versioned policy directly; source-v3 `retry`
lowers into that exact block contract. Cancellation, terminal timeout, approval denial, schema failure, ambiguous mutation,
and terminal failure are never eligible. A mutating owner-reported failure requires receipt/status
reconciliation; repair-required mutation is never automatically retried. The workflow store schema
version 6 persists one exact retry schedule per activation with failed/next attempt numbers, failure
kind, backoff duration, due timestamp, and scheduling timestamp. Owner reconciliation classifies the
terminal observation and atomically commits both terminal attempt state and an eligible schedule.
Scheduling is idempotent and never sleeps or creates the attempt; conflicting reschedules fail
closed. The bounded production driver consumes due schedules atomically, requeues only the exact
latest failed activation, and startup discovery includes failed runs carrying durable schedules.
Cancellation and stale or duplicate consumption fail closed.

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

Before attempt reservation, dispatch admission acquires every node-declared resource claim in
canonical resource-key order using stable activation-derived lease identities. Reader/writer
conflicts roll back the entire acquisition transaction, so parallel siblings never partially hold a
claim. Attempt reservation then enforces the persisted run concurrency cap before external dispatch.

Canonical fan-out results use version 1 `{ index, value }` members in strict contiguous ascending
input-index order. This shape is independent of completion order and rejects sparse or reordered
members. Production fan-out persists one member row per controller/input index with a stable
controller-derived activation identity, exact typed input, lifecycle state, output, and terminal
time. Initial admission is bounded by both the fan-out and run concurrency limits; waiting members
become pending in ascending index order as earlier members settle. Virtual member nodes retain the
owner operation's resources and use ordinary preparation, authorization, dispatch, reconciliation,
and cancellation paths. Canonical aggregation occurs only after every member succeeds. Fail-fast
failure persists cancellation intent for active siblings, cancels undispatched siblings, and fails
the controller/run; wait-all retains all admitted work before terminal failure. Reopening discovers
persisted pending members without rematerializing identities or inputs.

For supported wait-all joins, a failed member does not terminate the run while another member is
non-terminal. Once all declared members are terminal, the store persists one generation-scoped
ordered member-outcome decision and fails the run if any member failed or was cancelled.

For fail-fast joins, the first persisted failure atomically records a generation-scoped decision,
marks active sibling attempts with durable attempt-local cancellation intent, cancels siblings that
have not dispatched, and fails the run. Only after commit does the server signal exact runtime
owners. Successful signaling advances each attempt to `cancelling`; unsignalled intents remain
bounded and discoverable for startup retry. Each owner then reports the sibling's authoritative
terminal outcome through the normal attempt observation path. Both policies derive behavior from
persisted definition, activations, attempts, decisions, and events rather than ephemeral task order.

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

Git preparation, exact commit composition/approval, owner re-verification, and explicit commit
reconciliation are documented in [`git-workflow-blocks.md`](git-workflow-blocks.md).

## Bounded normal reads

List, status, open, and attach paths read bounded run/projection rows and paged workflow events.
They must not replay the complete workflow event history, scan every attempt, contact external
systems, or run repair. Summaries are projection-backed and include an explicit stale/degraded or
repair-required state when trust cannot be established.

## Reconciliation and repair

Automatic reconciliation is allowed only when durable receipts and owner APIs prove the current
operation state. Every production run persists an immutable target artifact plus a current daemon
coordinator generation and fencing token. Scheduling, continuation, and startup restoration qualify
that authority before entering a mutation cycle and recheck it throughout the cycle; stale or foreign
authority cannot dispatch, observe, cancel, resume, or terminalize the run. Authority transfer is a
same-artifact compare-and-swap to the next generation and occurs only after canonical session-owner
evidence proves the prior daemon ended. Agent-turn receipts additionally persist the exact daemon
artifact and daemon-instance identity that accepted the turn. A daemon with a different artifact, or
a replacement daemon while the recorded session owner remains live or unverifiable, defers
observation without mutating the attempt. This keeps the workflow database canonical across
artifact-isolated daemons without letting one artifact recover or terminalize another artifact's
live work. Prepared mutation without a trustworthy receipt or externally provable outcome becomes
`repair_required`; it is not retried automatically.

Full replay, projection rebuild, receipt investigation, forced retry, and ambiguity resolution are
explicit doctor/reconcile/repair operations. Maintenance acquires exclusive workflow-store
ownership and records its outcome. Normal read paths remain non-mutating even when the database is
damaged or stale.

## Clean-break schema and explicit reset

The workflow database has one supported schema version. A missing database is initialized directly
at that version. An existing database with an absent, malformed, older, or future contract is
rejected without writes; normal startup never migrates or reinterprets it.

Normal workflow startup opens the canonical store fail-closed. An incompatible, corrupt, or
maintenance-required workflow store disables only the workflow domain: daemon readiness and
unrelated session/model/tool capabilities continue, workflow requests return a stable unavailable
error, and the canonical workflow bytes remain untouched until explicit maintenance. The immediately
preceding schema has an explicit offline, backup-verified, non-destructive migration command;
unsupported older or damaged stores still require reviewed reset or future migration support. Core runtime,
model, auth, and session requests use separate typed routing and never pass through workflow
availability gates. Passive plugin session-status hydration treats an unavailable optional workflow
domain as no contribution rather than a session or skill failure. The unavailable domain uses only
an isolated process-local scratch store to satisfy internal construction; it is not canonical and no
workflow request or restoration path may reach it.

Destructive reset is a separate maintenance operation. It acquires the workflow ownership lock
exclusively (proving no workflow store handles are active), obtains an immediate exclusive SQLite
lock (proving no uncoordinated writer is active), creates a confined SQLite backup, verifies backup
integrity and records its SHA-256, removes only the canonical database sidecars and workflow-owned
artifact directory, initializes the current schema, and atomically writes a bounded reset receipt.
The backup is retained under `workflows/reset-backups/`. Reset refuses an absent or already-current
store and never runs from open, status, history, attach, or repair paths. The public maintenance
entry point is `bcode workflow reset-store --confirm DELETE-INCOMPATIBLE-WORKFLOW-STATE`; it runs
through the application/server boundary while offline rather than opening private persistence from
the CLI or requiring the daemon whose store is intentionally incompatible. The portable IPC request
exists only to return an actionable refusal from a running daemon; online reset cannot race the
daemon's live store ownership.

Operator status, doctor, shell/Git reconciliation, explicit repair, and backup-safe maintenance
procedures are documented in [`workflow-operations.md`](workflow-operations.md).

## Architecture enforcement

Once the durable package exists, `scripts/check-workflow-architecture.sh` must enforce at least:

* only the workflow persistence owner constructs `workflow.db`;
* session and loop packages do not define workflow tables or state files;
* normal workflow list/status paths do not call replay, repair, or external dispatch APIs;
* prepared intent precedes dispatch and validated output precedes downstream activation;
* ambiguous mutating attempts transition to repair-required rather than automatic retry.
