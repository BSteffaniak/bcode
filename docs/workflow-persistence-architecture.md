# Workflow Persistence Architecture

## Clean-break baseline and workflow coexistence

The supported baseline starts at storage compatibility contract 1 (schema 41).
Pre-contract binaries are outside the supported coexistence set. This work does not
engineer an online legacy bridge or a new production migration coordinator. Existing
backup-verified maintenance remains separate; no data deletion or daemon shutdown is
authorized by the clean-break decision.

Baseline daemon instances share one canonical workflow database. Idle shared handles
protect destructive maintenance, not admission or execution by other baseline owners.
Execution authority remains per run, with foreign and stale owners fenced out.

A server regression admits independent agent workflows through production `start_run`
using two distinct daemon instance identities and store handles, then runs both existing
drivers to completion. It uses the fake provider and one process; it is not yet a
multi-process daemon/IPC acceptance test. Storage subprocess tests separately cover
retained-handle writes and interruption safety. Neither claim includes old binaries.

## Existing historical-state handling

Known historical storage retains the existing exclusive, backup-verified initialization
path. Unknown or unsafe storage is preserved and refused. No online transition machinery
is a prerequisite for baseline coexistence. Future incompatible changes require their own
explicit architectural decision, not an assumption that old writers are safe.

## Storage compatibility boundary (partial implementation)

Schema 41 establishes compatibility contract 1 in `workflow_storage_compatibility`.
Fresh stores create it atomically; known historical stores through schema 40 enter it
through the existing exclusive, backup-verified transition. Normal opens require the
known compatibility contract, a revision of at least 41, and the required recovery,
continuation and package-binding structures. Higher revisions with that same contract
are not automatically incompatible and are not downgraded. Missing or unknown contracts
are refused. Retaining the contract across a future revision is a writer-semantics promise,
not merely a claim that its DDL is additive; incompatible semantics require a new boundary.
Reset refuses a structurally valid compatible higher revision as well as the current one.

Remaining verification concerns baseline multi-process daemon admission/execution and full
goal diagnostics, not a new additive migration coordinator or pre-contract mixed-version
acceptance. Existing shared handles still protect destructive maintenance.

Workflow requests can retry transient store initialization under a workflow-local mutex.
Contending callers receive a retryable-unavailable diagnostic immediately instead of queuing
behind the blocking initialization attempt; periodic driver discovery skips a busy initializer.
Successful initialization installs canonical storage before clearing unavailability and signals
the existing singleton driver to restore work. Startup restoration itself does not retry
initialization: this avoids restoring once during startup and again from the pending signal.
Permanent failures remain unavailable; startup and retry diagnostics use the same secret-safe
mapping. Concurrent retry publication is tested, but production admission, cancellation and
exactly-once restoration still require acceptance evidence.

## Package-local execution binding (partial implementation)

Schema 32 adds `workflow_run_packages`. Package export startup supplies an exact
package binding to atomic run admission, which checks membership and rejects changed
duplicate bindings. Existing runs remain unbound through the existing exclusive upgrade
coordinator. Explicit `PackageMember` call targets now verify member and compiled identity
against the parent's exact lock, require matching durable authority, and inherit the binding
in the child-admission transaction. Duplicate child admission verifies the binding; other
exact call kinds do not inherit it. Authored package-local calls now lower to
`PackageMember`, preserving the declared member ID and expected compiled identity through
publication and dispatch. Recursive resolution and demand-driven compilation
are not implemented; this storage path alone does not establish recursive execution. Dedicated bound-run
reopen, duplicate-conflict, and schema-31 upgrade acceptance still needs to be added.

Package-local calls resolve against an immutable run-owned `(package_id, lock_digest)`
binding, using the existing exact published package lock. No separate procedure-bundle
store is needed. The binding is execution data, never diagnostic authored provenance or
presentation metadata. Definitions retain local member references rather than embedding
their containing lock digest in their own content hashes.

Package export admission must validate the selected export against the exact lock and
persist the binding in the same transaction as run creation. A duplicate admission must
match the original binding; it cannot change a running execution's resolution context.
Package-local child admission resolves the member against that pinned lock and atomically
inherits the binding with child creation and parent linkage, under current execution
authority. Other exact-definition calls do not implicitly inherit package context.
Missing bindings, unsupported locks, and inconsistent member identities fail closed;
resolution must never substitute the latest package publication.

The durable schema upgrade must preserve existing runs as unbound, without inferring
bindings from authored provenance or current publications. Public and persisted contract
compatibility must be updated with the implementation. Recursive procedure reuse must
remain distinct from cyclic run ownership: each invocation creates a fresh child run,
subject to explicit configurable execution/resource policy. Compiler analysis must resolve
recursive references without recursively expanding unbounded preview paths.

## Pending publication storage (partial implementation)

Schema 31 adds pending acceptance and exact attempt-intent records. The store acceptance
operation validates the staged candidate under current authority and commits acceptance
with receipt-backed, unlinked cancellation targets atomically. Duplicate acceptance keeps
the original attempt set; failed target validation rolls back all acceptance writes.
Schema-30 initialization upgrades preserve existing runs without inventing intents.
The invocation application boundary now advertises `accept_run_graph_publication`, using
publication grants, exact candidate matching, authenticated execution provenance, and
transactional active-caller verification. It reserves a driver wake before acceptance and
returns the typed lifecycle status; the existing publication operation stays committed-only.
Local-client acceptance is now exposed through appended IPC request/response variants and
a client method, sharing publication policy checks and exact-candidate authorization.
Scheduler capacity is reserved before acceptance; the store returns duplicate lifecycle
status atomically. Full local-client acceptance integration coverage, pending-path execution,
and recovery remain unfinished. The normal daemon driver now
signals discovered publication cancellations using its held authority and transactionally
marks successful signalling. Cancellation discovery now advances by dispatch-identity
keyset pages within each drive, so unresolved first-page owners do not hide later targets.
A completed cancellation sweep waits for a subsequent drive before retrying missing owners.
Missing runtime owners leave the intent pending for receipt
reconciliation rather than manufacturing terminal evidence. Accepted
attempt intents now participate in owner-observation settlement and authority-qualified,
bounded cancellation discovery. Prompt completion and receipt observation now classify a
confirmed cancelled model turn as Cancelled when its exact dispatch has an accepted
publication cancellation intent, rather than incorrectly pausing it for steering. Ordinary
steering without such intent still pauses. Running observations remain pending; confirmed cancellation
removes the attempt from discovery without requesting cancellation of the whole run.
The authority-qualified status read now projects pending, conflicted, or committed outcomes
from a bounded snapshot. Prompt receipt reconciliation resolves output schemas from the
activation's admitted graph node rather than its authored definition, so added/replaced
executables retain their own result contract. Receipt schema identity must match admission.
Missing execution sessions and history pages with compatibility issues produce an unknown
observation, not a fabricated running or cancelled state. Authority-qualified reconciliation
therefore retains unresolved work or requires repair without using missing history to
finalize pending publication. Later publications conflict accepted candidates without deleting
cancellation intents; prior committed results remain stable. The invocation acceptance
operation returns this projection on duplicate requests. Store finalization now requires
exact accepted cancellation targets to have confirmed cancelled attempts and activations,
then revalidates and publishes atomically under current authority. Pending and conflicted
results do not publish; committed duplicate results are stable. The daemon driver now
sweeps accepted identities in keyset pages after receipt reconciliation and finalizes pending
candidates under its held authority. A new commit keeps scheduling active for the revised
graph. An integration test now accepts a fresh candidate through the authenticated server
invocation method, reopens the store, rediscovers the run, supplies confirmed cancellation,
and invokes daemon finalization to create pending revised work. This is not a complete
restart or owner-signalling exercise: cancellation evidence is injected by the test and
successful revised execution is not asserted. Existing committed-only publication
operations are unchanged.

The bundled workflow plugin exposes `workflow.accept_run_graph_publication` through its
normal mutating-tool preparation and invocation bridge. Its prepared facts bind the exact
operation and candidate. Pending responses explicitly deny publication, conflict responses
preserve cancellation and require a revised candidate, and malformed/future results report
unknown outcome. Existing staging and committed-only publication tools remain unchanged.

## Pending publication decision (approved; not yet implemented)

An accepted publication that requires owner cancellation must expose a durable pending
outcome rather than pretending that a graph revision has committed. It identifies the
exact authorized candidate, expected graph revision, and affected attempts. Acceptance
and the corresponding cancellation intents must commit together before owner signalling.

Later authorized graph edits remain permitted. If the expected graph revision changes
before final publication, the pending publication becomes conflicted; it must not silently
rebase or publish against the new revision. Cancellation already requested remains durable,
visible history and is not undone. Retrying after conflict requires an explicitly revised
candidate with a new mutation identity. Repeating the original identity reports its existing
outcome, and conflicting duplicate payloads reject without additional effects.

Final publication must verify current execution authority and exact owner settlement,
revalidate the candidate's execution bindings, and commit the revision with its terminal
publication outcome. A missing runtime entry or unknown owner observation is not settlement
proof. Lost-admission recovery remains ownership-qualified and must preserve ambiguity.
Pending, conflicted, and committed outcomes must be represented consistently through the
application, invocation, and client boundaries. Existing committed-only operations must
not begin returning errors after secretly accepting durable cancellation side effects.

These are the approved semantics for the outstanding implementation, not capabilities
provided by the current committed-only publication path.

## Durable dispatch handoff

Schema 30 adds attempt-keyed handoff evidence through the exclusive upgrade coordinator.
Only `prepare_pending_activation` creates never-handed-off proof. Fresh dispatch and both
read-only redispatch paths mark handoff before awaiting an owner. Publication cancellation
serializes against that marker, terminalizes only proven never-handed-off preparations,
and releases activation resource leases. Later handoff and receipts reject cancelled attempts.
No evidence is fabricated for historical or low-level preparations. Receipt-less mutating
recovery remains conservative, including after handoff; the marker is not an owner receipt.
Handed-off work and linked execution still require owner reconciliation and remain guarded.
The normal daemon driver passes held execution authority into pending dispatch; handoff
rechecks it transactionally. Resource acquisition now shares preparation's authority-checked
transaction and uses the exact pending activation resolved there. Failed admission rolls back
new leases and their events; the scheduler does not commit a separate resource-acquisition phase.
Startup recovery uses caller-qualified redispatch with expected-authority checks at discovery,
handoff, and receipt commit. Normal continuation retries the same bounded read-only
redispatch path when startup owner access was deferred; dispatch identities are retained.
A deferred owner does not block unrelated pending work, and authority is verified again
before continuation. Mutating receipt-less work is not redispatched by this retry.
Startup preparation recovery also checks held authority within
its classification/mutation transaction. Historical orphan lease reconciliation and handed-off
owner cancellation remain outside this new-admission guarantee.

If cancellation signalling wins the race with owner receipt persistence, the receipt can
still be recorded on a nonterminal cancelling attempt without restoring admitted status.
Identical duplicate receipts are idempotent; conflicting receipts and receipts for terminal
attempts reject. This preserves owner evidence for reconciliation, but does not authorize
publication to replace handed-off work or establish an owner-admission cancellation fence.
Missing runtime work cannot settle a receipt-less attempt with durable handoff evidence:
orphan cancellation leaves it unchanged and discoverable until owner evidence arrives.
A permanently lost admission still requires ownership-qualified recovery; this deferral is
not proof of orphanhood and does not complete the cancellation protocol.
Normal daemon cancellation propagation rejects missing/foreign execution authority before
owner signalling and passes the captured authority to transactional signal marking and
orphan settlement after the await. Stale authority rejects those writes. This does not yet
fence all initial intent writes, recursive/sibling signalling, or graph-edit cancellation.
Associated-run Cancel control now persists intent transactionally under its held authority
and passes that same authority through pre-signal verification and post-signal persistence.
The public tree-cancellation path resolves and retains each selected run's authority,
uses owned intent writes, and passes those authorities into propagation. Selection remains
a bounded descendant snapshot, not an atomic tree operation; newly admitted descendants
and recursive/sibling owner signalling still require reconciliation.
Cancellation intent does not prove owner termination. For every owner kind, admitted or
running observations retain cancelling state, and deferred or unknown observations leave
settlement pending. Only terminal owner observations can settle cancellation. This also
applies to fail-fast sibling intent; a running sibling must not be recorded as stopped.

## Workflow-owned gate cancellation

Graph publication can cancel waiting input and approval gates without attempts or linked work.
The workflow store owns these waits; publication and answer resolution serialize through its
transactions. Cancellation removes the gate from waiting projections, and late answers after
reopen cannot produce outputs or successors. If an answer commits first, publication rejects
its stale cancellation disposition and preserves the output and successor. Prepared attempts,
linked work, and mutation-approval waits remain rejected pending operation-owner reconciliation.

## Connected direct-chain publication

The existing authenticated application and invocation publication paths now accept direct chains
with explicit retained-source bindings, schema-identical untransformed edges, and targets that are new or explicitly retained active work behind unchanged
incoming edges. Retention never changes admitted inputs or executable revisions; completed and
cancelled targets remain rejected. Candidate validation, retention, cancellation, graph publication, and
admission remain one transaction. Existing admission identities remain historical. Controller
nodes, joins, fan-out, and changed or terminal admitted targets remain rejected; this is a bounded supported
subset, not general connected reconciliation. The existing leaf-named store methods retain their
names for compatibility but also accept this subset.

Store coverage publishes a replacement successor through staging/publication, reopens storage,
settles the retained source, and completes the revised successor. Application coverage checks
invocation authorization, exact candidate identity, duplicate publication, and wake delivery.
Daemon coverage publishes a new two-agent chain alongside retained work, discards the wake,
and observes durable discovery dispatch both agents to validated output using the fake provider.
This establishes the new-chain worker path, not process-loss recovery or changed-active-work reconciliation.

## Retained edge publication storage

Schema 29 adds revision-scoped retained-edge bindings through the existing exclusive upgrade
coordinator, including upgrades from schema 28. Publication records the exact source activation
and committed edge revision atomically with its publication receipt. Foreign keys preserve those
historical identities; one edge has at most one retained source per graph revision. The original
storage addition did not enable connected revised execution; the supported direct-chain subset
is described above.

## Retained leaf publication

Schema 28 adds revision-scoped leaf retention records through the existing exclusive upgrade
coordinator (including schema 27). `publish_retained_leaf_run_graph_edit` atomically records
explicit retention of every active, unchanged leaf alongside publication. Admission revisions
remain immutable; revised leaf settlement accepts a retention record for the current revision.
Connected graphs, changed active nodes, cancellation dispositions, and result-edge bindings
remain rejected. This store capability is not yet exposed through application/tool publication.

## Quiescent leaf publication

Schema 27 adds durable publication outcomes keyed by staged mutation identity. The store's
`publish_quiescent_run_graph_edit` revalidates candidates inside the publication transaction,
checks current execution authority, rejects active activations and explicit reconciliation,
and atomically retires/replaces node revisions, advances the expected graph revision, records
the outcome, and appends an event. Duplicate publication returns the original revision without
writes, including after reopen. Exclusive backup-verified upgrades now include schema 26.

This is a restricted store capability, not the complete live-edit application path. It rejects
runs with any historical edges and candidates with surviving edges because revised successor
settlement is not implemented. Active-work dispositions, result bindings, operation-owner
coordination, and application integration remain outstanding; existing execution gates stay in place.

## Staged live graph edits

Schema 19 adds workflow-owned edit candidates. `stage_run_graph_edit` atomically retains a
bounded typed request, its admitting execution authority, and a staging event. Identical duplicate
delivery is non-mutating; conflicting mutation IDs and stale revisions fail closed. Migration from
schema 18 uses the existing exclusive, backup-verified migration coordinator and preserves activation
bindings. Older supported migrations still initialize bindings before adding candidate storage.

A candidate is **not executable graph authority**. Staging does not change the committed graph
revision or reconcile active work. `validate_staged_run_graph_edit` reads one graph snapshot,
applies candidate operations in memory with identity preconditions, and invokes workflow structural
validation. Graphs exceeding one bounded page explicitly require incremental validation; that path is
not yet implemented. Schema 20 persists successful bounded structural validation keyed by candidate
identity and expected graph revision; duplicate validation replaces the same record after revalidation.
The existing exclusive migration coordinator upgrades schema 19 without changing candidate requests
or activation bindings. These records are not publication authority and cannot bypass ownership,
revision, permission, or reconciliation checks. General candidate
publication, lifecycle management, and application integration remain unimplemented; the restricted
quiescent leaf publisher above is the only publication path. Dispatch safety gates remain in place.

## Retired graph entities

Schema 21 adds retirement revisions to node and edge records without deleting their executable
payloads. Current reads exclude retired entities through partial indexes before applying page limits;
exact historical revision reads retain the original payloads. Historical edge reads require endpoints
that were live at the edge's revision: retirement does not invalidate older edges, and later
reintroduction cannot legitimize an edge created while its endpoint was absent. Indexed retirement checks reject
uncommitted retirement revisions rather than hiding them from pages. Publication must retire every
previously live record of a removed identity in the same transaction that advances the graph revision;
reintroduction uses a new executable revision. The existing exclusive, backup-verified upgrade path
preserves schemas 14–20 and initializes existing records as unretired. Normal reads never migrate.

This representation is a publication prerequisite, not an edit publication API. Atomic publication,
active-work reconciliation, and incremental candidate validation remain unimplemented.

## Activation admission graph bindings

Schema 22 records the admitted graph revision separately from the executable node revision for
`create_activation_at_graph_revision`. Both records commit in the ownership-fenced admission
transaction. An unchanged node can retain revision 1 while its activation is admitted at graph
revision 2. Bounded `activation_admitted_graph_revision` reads validate the binding against the
committed graph and executable node; they preserve caller-owned transactions.

The exclusive upgrade path supports schemas 14–21 without inventing admission facts. Earlier
activations have no recorded graph binding and return `None`; callers must not substitute the node
revision. New initial-graph admissions, successor creation, branch skips, and newly inserted fan-out
members record graph revision 1 in their existing transaction. Duplicate fan-out delivery does not
backfill historical admission facts. Revision-aware admission records its verified expected revision.
Initial-only execution lookups reject recorded admission bindings other than revision 1 before
preparation or settlement mutation. Missing historical admission facts retain the existing validated
initial-executable path; they are not persisted or inferred. Scheduler integration for edited graphs
and publication remain unfinished.

## Validated candidate deltas

Schema 23 persists normalized candidate node/edge deltas in the structural-validation transaction.
Only identities touched by the bounded edit request are stored; unchanged graph portions are not
copied. Payloads contain the final validated state after all operations, with null payloads denoting
removal. Duplicate validation upserts the same candidate identities. These records reference the
candidate validation revision and are not executable graph authority. Older validation markers have
no inferred deltas: revalidation is required before a future publisher can consume them. Exclusive
backup-verified upgrades now support schemas 14–22. Publication and reconciliation remain absent.

## Reconciliation target validation

Schema 24 adds a run/activation identity index for bounded reconciliation lookup. Candidate validation
requires each explicit retain/cancel target to resolve to exactly one nonterminal activation without
an output in the candidate's run. Invalid, missing, ambiguous, or settled targets fail before validation
writes. This is snapshot validation only: publication must recheck targets and apply dispositions
atomically; no cancellation or retention is performed here. The exclusive upgrade path supports
schemas 14–23. Schema 25 adds a partial running-node index and requires explicit dispositions for
running activations of changed nodes and old/new targets of changed edges during bounded candidate
validation. The lookup stops after the maximum disposition count plus one; it cannot silently accept
an uncovered running activation. Within the bounded graph snapshot, affected identities expand
transitively over current edges and candidate-added/replaced edges, with visited identities preventing
cycles from looping. Completed activations remain untouched. Schema 26 adds an indexed lookup of
running synthetic fan-out members by affected controller, requiring their explicit dispositions within
the same bounded coverage check. Operation-owner fencing remains uncovered, and validation does
not apply dispositions. Publication must perform
complete execution-aware reconciliation under current authority. Legacy activation insertion now
rejects graphs beyond revision 1 rather than silently binding initial nodes; exact-revision admission
remains the supported store capability for revised graphs. Repeat/join/output settlement still reads
initial topology. Revised leaf output settlement now accepts an activation explicitly admitted at
the current graph revision when its bound node is still the latest non-retired representation and
there are no outgoing edge records. It uses that node's exit flag and preserves transactional output
completion. Historical admissions and any outgoing edges still require reconciliation; this narrow
path does not enable publication, repeat/join settlement, or operation-owner coordination.

### Retained-result binding decision

Retaining an in-flight activation preserves its admitted executable and inputs; it does not by
itself authorize output reuse across newly connected or changed dependencies. The live-edit contract
must represent explicit compatible bindings from retained activations to revised dependencies.
Publication must validate these bindings against the candidate graph and immutable source executable,
then commit them with reconciliation intent. Settlement must consume the authorized bindings without
changing historical result identity. Matching node IDs or current topology alone confer no reuse
authority. Existing staged version-1 candidates must be preserved without acquiring implicit bindings.
This decision is approved. Live-edit version 2 now represents `RetainWithBindings` using an
activation identity and a bounded list of candidate edge identities. Version 1 remains accepted
without bindings and rejects the new disposition; old candidates gain no implicit authority.
Staging preserves binding intent in the existing bounded request payload. Candidate validation
requires each bound edge to originate at the retained executable's node and its target input to
exactly match the immutable source output schema; transforms are rejected until compatibility can
be proved. Publication and settlement consumption of these bindings remain outstanding.

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

Live admission uses the same recovery policy. The daemon holds an ephemeral per-run drive gate
across admission and receipt commit; periodic recovery acquires that gate without waiting before
classifying receipt-less attempts. Durable execution authority remains mandatory: the gate is
not cross-daemon authority. An abandoned read-only preparation is redispatched with its original
identity through the existing owner contract; an ambiguous mutating preparation becomes
repair-required rather than being replayed. Cancellation fences still apply before handoff.

`workflows.admission_timeout_ms` (positive, default 30000) bounds the complete owner admission
future, including session-context and shared-session permit acquisition. Shutdown also interrupts
admission. Neither interruption nor timeout proves non-acceptance; prepared intent remains for
ownership-qualified reconciliation. This deadline does not bound workflow lifetime or model
execution after admission. `workflows.continuation_workers` (positive, default 16) bounds concurrent
background run continuations, so a blocked admission does not monopolize the discovery worker.
The worker set is supervised and dropped on shutdown; durable discovery retries abandoned work.

Git preparation, exact commit composition/approval, owner re-verification, and explicit commit
reconciliation are documented in [`git-workflow-blocks.md`](git-workflow-blocks.md).

## Bounded normal reads

List, status, open, and attach paths read bounded run/projection rows and paged workflow events.
They must not replay the complete workflow event history, scan every attempt, contact external
systems, or run repair. Summaries are projection-backed and include an explicit stale/degraded or
repair-required state when trust cannot be established.

## Reconciliation and repair

Automatic reconciliation is allowed only when durable receipts and owner APIs prove the current
operation state. Every production run persists its current coordinator artifact plus a daemon
coordinator generation and fencing token. Recovery barriers separately retain the original
execution artifact; coordinator replacement does not imply execution compatibility. Scheduling,
continuation, and startup restoration qualify
that authority before entering a mutation cycle and recheck it throughout the cycle; stale or foreign
authority cannot dispatch, observe, cancel, resume, or terminalize the run. Authority transfer is a
compare-and-swap to the next generation and occurs only after canonical evidence proves the prior
daemon ended. Two forms exist:

* **Same-artifact transfer** preserves the target artifact and requires session-owner evidence that
  the prior coordinator ended. It is the normal replacement-daemon path and may recover live work.
* **Cross-artifact recovery takeover** changes the coordinator and atomically installs a
  recovery-only barrier, preserving unresolved attempts and their original artifact. Positive
  ended-owner evidence is required; missing records are unverifiable. New attempts, dispatch
  handoff, resume, and child admission are blocked. Receipt observation and authorized cancellation
  remain possible, but incompatible receipts defer. Explicit resume can clear the barrier only
  with settled attempts, terminal linked children, and the original execution artifact. Barrier
  removal and resume commit together. The quiescent reassignment store operation remains available,
  but application ownership resolution uses recovery-only takeover instead.

Agent-turn receipts additionally persist the exact daemon artifact and daemon-instance identity that
accepted the turn. A daemon with a different artifact, or a replacement daemon while the recorded
session owner remains live or unverifiable, defers observation without mutating the attempt and
reports the owning daemon in a typed `workflow_owned_by_live_daemon` error so operators can act on
it. This keeps the workflow database canonical across artifact-isolated daemons without letting one
artifact reinterpret another artifact's private receipts. This is not yet complete crash recovery:
exact lifetime-independent outcome lookup, cross-artifact operation reconciliation, and durable
replacement remain unfinished. Prepared mutation
without a trustworthy receipt or externally provable outcome becomes `repair_required` on the
compatible startup path; recovery-only takeover preserves it without guessing. It is not
retried automatically.

A paused run holds no live process resources. Pausing suspends the run's session-level runtime
work registration (`RuntimeWorkStatus::Suspended`) so the coordinating daemon can reach quiescence,
release session ownership, and shut down when idle; resuming re-registers the work under the same
identifier. Daemon startup settles run-level registrations inherited from a prior daemon for every
quiescent (terminal or paused) run.

`bcode workflow reconcile-orphans` is the explicit maintenance operation for nonterminal runs whose
coordinator verifiably ended. It reports each candidate with its evidence and, only with `--apply`,
acquires qualified authority and requests cancellation (a `repair_required` run retains its status
for attempt-level repair). Foreign-artifact takeover remains recovery-only. Cancellation acceptance
is not proof of termination. Runs owned by a live or unverifiable daemon are always skipped.

Full replay, projection rebuild, receipt investigation, forced retry, and ambiguity resolution are
explicit doctor/reconcile/repair operations. Maintenance acquires exclusive workflow-store
ownership and records its outcome. Normal read paths remain non-mutating even when the database is
damaged or stale.

## Schema upgrades and explicit reset

The workflow database has one current schema version. A missing database is initialized directly
at that version. Domain-owned startup coordination automatically upgrades supported schemas 14–39
under the migration safety contract in `INVARIANTS.md`. Schema 40 adds atomic successor-continuation
lineage and an indexed lookup of structured exhaustion events. A continuation never reopens its
terminal predecessor: source ownership, exact output/graph checkpoint, current association,
quiescence, and explicit allowance are checked in the same transaction as successor admission.
Lineage retains cumulative prior iterations and the original working-document scope without
walking predecessor chains. Request identity defines exact retry/conflicting-duplicate behavior.
Normal inspection reads bounded lineage/checkpoints; it does not replay or repair history.
Schema 34 adds durable recovery barriers
and dispatch/resume triggers; current-schema opens fail closed when required barriers are missing.
Schema 33 supplies the run-package binding
table omitted by earlier upgrades. If that table is absent, automatic creation requires an empty
publication catalog; existing publications make the missing bindings ambiguous and require explicit
maintenance. Existing binding rows are preserved. Current-schema opens validate the required table
without creating it. Ordinary store opens and status/history
reads never migrate. Unsupported, malformed, or future contracts fail closed without reset.

Workflow initialization starts eagerly on a blocking worker, independently of daemon readiness.
Connection and ordinary session operations never await it; workflow requests receive an immediate
initializing or unavailable result until the canonical store is ready. A workflow request may
schedule one background retry of a transient failure; concurrent callers do not queue behind it.
Ownership acquisition is nonblocking: contention returns immediately, without sleeping or polling.
Initialization rechecks the schema after acquiring exclusive ownership and retains ownership through
migration and reopening. Competing initializers may retry and share the completed current-format
store. Existing owners are never terminated
or revoked to obtain migration access. A SQLite write reservation spans the verified backup and
transactional schema/data conversion, preventing an uncoordinated writer from changing the source
between backup and commit. Integrity is verified before commit. Interrupted transactions roll back;
retained backups are not overwritten, and a retry uses a fresh backup name. A committed migration
can reopen even if receipt publication was interrupted; the schema transaction remains authoritative.

An incompatible, corrupt, blocked, or maintenance-required store disables only the workflow domain:
daemon readiness and unrelated session/model/tool capabilities continue. Workflow requests return
normalized actionable unavailable diagnostics without exposing private storage errors. Startup never
resets state. The explicit `bcode workflow migrate-store` command uses the same migration engine;
unsupported older or damaged stores require reviewed maintenance or future migration support. Core runtime,
model, auth, and session requests use separate typed routing and never pass through workflow
availability gates. Passive plugin session-status hydration treats an unavailable optional workflow
domain as no contribution rather than a session or skill failure. The unavailable domain uses only
an isolated noncanonical placeholder to satisfy internal construction (in-memory during daemon
startup); no workflow request or restoration path may reach it. Recovery and continuation discovery
are performed by the workflow driver after initialization, not by the daemon's readiness path.
Embedded workflow hosts explicitly await initialization and restoration before their workflow-ready
callback because that callback directly requires the capability.

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

Domain-owned behavioral, compatibility, and dependency checks should cover:

* only the workflow persistence owner constructs `workflow.db`;
* session and loop packages do not define workflow tables or state files;
* normal workflow list/status paths do not call replay, repair, or external dispatch APIs;
* prepared intent precedes dispatch and validated output precedes downstream activation;
* ambiguous mutating attempts transition to repair-required rather than automatic retry.
