# Composable coding workflows

## Purpose

Composable coding workflows turn Bcode's durable workflow engine and runtime-authoring contracts into
a product for long-running coding automation. The flagship product creates or attaches a local
progress document, performs bounded implementation batches, validates and formats exact repository
states, creates approved Git checkpoints, evaluates completion, and refocuses the plan between
batches. Manual graph editing, prompt-generated drafts, CLI clients, SDK clients, and future
frontends all produce the same versioned `WorkflowAuthoringDocument`.

This design extends the existing workflow domain. It does not add another scheduler, graph format,
canonical store, permission model, or frontend-defined execution path.

## Product decomposition and fixed limits

The flagship product uses three immutable workflow levels:

1. An **implementation batch** runs at most 20 implementation iterations and returns either a
   completed or exhausted typed result.
2. A **delivery tranche** calls at most five exact implementation-batch revisions, refocusing the
   progress document after each exhausted batch.
3. A **progress-driven delivery workflow** calls at most ten exact tranche revisions and waits for
   explicit operator continuation between tranches.

The resulting hard limits are 50 batches, 1,000 implementation iterations, four workflow nesting
levels, and 64 descendants per root run. Counters are derived from durable repeat and child-run
history; model output never increments or authoritatively supplies them. Continuing after the hard
limit requires a new run.

## Ownership

### Generic workflow domain

`bcode_workflow` owns only portable workflow semantics:

* typed definitions, nodes, edges, schemas, state envelopes, predicates, and transforms;
* versioned repeat outcomes and runtime-owned count contracts;
* exact workflow-call targets and portable dependency descriptions;
* semantic authoring edits; and
* normalized capability, requirement, effect, resource, and permission previews.

It must not contain `CodingWorkflowState`, progress-document behavior, repository implementation,
formatter-specific behavior, renderer logic, daemon types, or private plugin types.

`bcode_workflow_store` remains the sole canonical execution store. It owns runs, activations,
attempts, receipts, outputs, decisions, grants, events, canonical terminal output, parent/child links,
restart reconciliation, and explicit repair. It does not interpret coding-product state.

The server application boundary resolves catalogs and exact child dependencies, authorizes
operations, coordinates child admission, schedules generic work, dispatches agents and plugin blocks,
routes portable interactions, and exposes IPC/client operations.

### Product and operation owners

The bundled, disableable workflow plugin owns the versioned `CodingWorkflowState`, flagship workflow
documents, product outcomes, authoring assistance, and product vocabulary. It composes capabilities
but does not own shell, Git, progress-file, persistence, or scheduler behavior.

The shell plugin owns process plans and results. The Git plugin owns repository snapshots, Git status
interpretation, exact staging, commits, and Git reconciliation. A concrete bundled but disableable
progress-document plugin owns confined progress-file inspection, creation, replacement, parsing,
digesting, and reconciliation.

A domain-owned project-instruction capability resolves applicable `AGENTS.md` files. The current
server-local lookup must migrate to that capability when it is implemented so agent context and
workflow validation-plan generation use one semantic source. No speculative package is created
before that concrete capability exists.

## State-preserving operation dataflow

`WorkflowStateEnvelope` remains the generic explicit state carrier. Nodes gain a versioned dataflow
policy:

* `direct` preserves existing behavior; the complete node input is dispatched and the owner result is
  the complete node output.
* `state_envelope_v1` validates the complete envelope, dispatches only its `value` to the owner, and
  rewraps the validated owner result with the retained `state` and bounded artifact references.

For envelope mode the host performs these steps in order:

1. Validate the envelope version and complete node-input schema.
2. Bound serialized retained state and artifact references.
3. Validate the unwrapped value against the owner contract.
4. Derive permission facts from the unwrapped owner operation, never retained state or presentation.
5. Complete authorization before owner dispatch.
6. Dispatch only the unwrapped value in the owner invocation.
7. Validate the owner result against the owner output contract.
8. Rewrap and validate the complete node-output schema before persistence.

The owner cannot mutate hidden host state. Retained state is explicit in schemas, transforms,
checksums, history, and inspection. Unknown policy versions fail closed.

## Deterministic predicates

The finite predicate contract grows through a new explicit version while preserving version 1
constant equality. The supported operations are:

* constant equality;
* field-to-field equality;
* bounded `all`, `any`, and `not`; and
* numeric less-than, less-than-or-equal, greater-than, and greater-than-or-equal when both referenced
  schema paths are numeric.

Predicate trees are bounded to depth 16 and 256 operations, matching the transform budget unless a
future measured limit replaces both coherently. Compilation validates referenced paths and compatible
JSON categories. Evaluation has no scripts, plugins, model calls, filesystem access, network access,
time, or randomness. Unknown operations and versions fail closed.

## Repeat outcomes

Existing repeat behavior remains the default: if the predicate still requests another iteration when
the effective bound is reached, the run fails with `repeat_iteration_limit_exhausted`.

A new `emit_outcome` exhaustion policy returns a versioned generic result containing:

* `condition_cleared` or `iteration_limit_reached`;
* iterations completed;
* definition maximum;
* effective run-level cap; and
* the retained typed value.

Settlement and the outcome commit atomically. Runtime-owned activation generation is the count source.
Stale settlement cannot replace a terminal outcome. The result is ordinary typed workflow data and
may route to a child caller or refocus branch.

## Canonical terminal workflow output

Successful workflow composition requires one authoritative terminal value. Each run therefore gains
an optional canonical terminal-output pointer and checksum.

The exact successful exit activation atomically commits the pointer, checksum, completed status, and
terminal event. Byte-equivalent duplicate settlement is idempotent. A conflicting terminal value,
multiple ambiguous successful exits, missing output, or schema mismatch fails closed. Once committed,
stale live or recovery updates cannot replace it. Failed, cancelled, paused, or repair-required runs
cannot expose successful terminal output.

Normal inspection returns only bounded schema identity, checksum, artifact reference, and value
preview. It does not load complete artifacts or replay history.

## Exact child workflow calls

`WorkflowCall` is a first-class generic node. Version 1 is synchronous and accepts either:

* an exact published authored revision plus expected compiled definition identity; or
* an exact immutable registered definition identity.

An exact preset generation may be pinned. Active-revision lookup is forbidden during dispatch.

Validation and publication resolve the complete dependency graph within the depth-four and
64-descendant bounds. They reject direct or indirect recursion, unavailable definitions, unsupported
future versions, schema mismatch, ambiguous identities, and oversized dependency previews.

Child admission follows this durable order:

1. Validate the parent activation and exact target.
2. Derive a deterministic child run identity from root run, parent run, activation, attempt, and
   target identity.
3. Atomically create or verify the child and persist the parent/child link.
4. Persist the child identity as the accepted parent-attempt receipt.
5. Schedule the child without retaining parent resource leases that the child may need.
6. Observe events or bounded owner status until a stable child outcome; do not unboundedly poll.
7. Validate canonical child output against the call-node output schema.
8. Persist the parent node result before activating successors.

Duplicate delivery must resolve to the same child identity and exact target. A conflicting duplicate
fails closed. Restart reconciliation uses the persisted link and receipt rather than creating another
child.

Parent cancellation requests child cancellation and waits for a stable child outcome. Version 1 does
not abandon children. Child workspace identity must equal the parent's. Child operations inherit the
initiating authorization ceiling but not ambient grants; a root grant applies only when it explicitly
names the exact descendant definition, node, operation, plan digest, tranche, use bound, and expiry.

Parent preview recursively aggregates bounded child plugins, skills, capabilities, effects,
resources, command plans, model selections, explicit grants, mutation approvals, depth, and descendant
count. Export/import carries an exact dependency manifest and never substitutes dependencies.

## Project instructions and reviewed command plans

The project-instruction capability resolves candidates from repository root to each relevant target,
preserving precedence. It returns normalized paths, presence or absence, SHA-256, and bounded content.
The union is recomputed for selected changed paths. Traversal, symlink escape, malformed encoding,
oversize, unsupported future state, or ambiguous repository ownership fails closed.

An exact workflow revision or preset stores validation and formatting plans, their provenance, the
applicable instruction fingerprint, and a drift policy. The default is `block_and_review`.

If applicable instructions drift, the workflow does not run the stale plan. It persists a typed drift
receipt, asks a read-only planning agent for a replacement, shows exact instruction and command
changes, waits for operator acceptance, and persists the accepted plan as a run decision. That exact
decision is reused for the rest of its declared scope.

## Shell plans

Shell command-plan version 1 remains accepted unchanged. A new version adds bounded, unique accepted
exit codes to each command; the default is `[0]`. An exited code outside that set is typed command
failure. `continue_on_unaccepted_exit` controls later command execution. Spawn failure, timeout,
signal termination, cancellation, and unaccepted exit remain distinct.

Results include actual and accepted exit codes, terminal status, duration, bounded stdout/stderr
previews, truncation flags, and artifact references. Shell plans remain exact argv arrays and never
become implicit shell strings.

## Repository snapshot and verification authority

`workspace_snapshot` continues to identify the immutable workspace location for workflow and grant
scope. It is not repository content identity.

The Git plugin owns a versioned `RepositorySnapshot` containing:

* canonical repository identity and HEAD object ID;
* ordered include and exclude policies;
* ordered changed entries with index/worktree status, base/index/worktree identity, file mode, and
  entry kind;
* deletion markers, rename/copy source, symlink target identity, submodule identity/dirty state, and
  untracked content identity;
* applicable project-instruction fingerprint; and
* canonical aggregate SHA-256.

Git-owned parsing uses porcelain-v2 `-z` or an equivalently unambiguous representation. Unsupported
non-portable paths fail closed in version 1 instead of being lossily decoded. Authoritative manifests
are bounded; overflow returns `scope_too_large`, never a truncated identity.

A verification receipt is authoritative only when every required command satisfies its accepted-exit
contract, pre- and post-command snapshots are identical, the instruction fingerprint matches the
accepted plan, required artifact evidence is complete, and the receipt records exact plan and
snapshot digests. Any later included mutation invalidates it. Excluded local-only paths are recorded
but do not invalidate code verification.

Formatting is a separate mutation. The required order is pre-verification snapshot, verification,
unchanged check, formatting, formatted snapshot, post-format verification, unchanged check, and final
verified snapshot. Formatter selection is configuration; the runtime never special-cases Clippier.

## Exact Git checkpointing

A commit request contains expected HEAD, final verified snapshot digest, exact included and excluded
paths, expected selected-entry manifest, and structured title and description.

Immediately before mutation the Git owner recomputes the selected snapshot, rejects stale HEAD or
changed content, rejects unrelated staged paths, stages only exact selected paths, and verifies index
entries against expected Git object identities. It commits only those paths, verifies parent and
committed path set, and returns a typed receipt. Ambiguous accepted mutation remains repair-required.
The progress-document path is excluded automatically.

Commit approval remains exact and per checkpoint. Validation and formatting plans may instead use a
tranche-scoped grant only when it binds root run, descendant definition/node, workspace,
plugin/block/operation, exact plan digest, tranche, maximum uses, and expiry. Use count commits before
each dispatch. Progress writes and commits cannot use this broader grant.

## Progress-document interaction and persistence

The default path is repository-root-relative `local-<workflow-slug>-progress.md`. User-supplied
repository-relative paths take precedence. The exact path is part of workflow state and commit
exclusions.

Progress creation and refocus use required `local-progress-doc` and `refocus-progress-doc` skills in
shared-parent sequential agent turns. The agent produces an exact path and payload preview but does
not silently write from a background turn. The proposal is surfaced in the active parent session and
resolved through a durable renderer-neutral interaction with `Apply`, `Revise`, and `Cancel`:

* Apply records approval provenance and forwards the exact payload to the progress plugin.
* Revise records bounded guidance and returns to the drafting agent.
* Cancel produces the configured paused or stopped product outcome.

Normal mutation authorization still precedes writing.

The progress plugin provides `inspect`, `create`, `replace`, and `reconcile`. Mutating requests contain
expected absence or current digest, desired content and digest, repository-relative path, and approval
identity. Paths are canonicalized and confined before access. Writes use a safe temporary file and
rename. Reconciliation compares expected previous and desired final digests.

Version 1 bounds documents to 512 KiB and 4,096 task items. Inspection returns path, digest, byte
length, checked/unchecked/total counts, parse-complete state, at most 128 unresolved summaries, and a
truncation indicator. Malformed encoding, incomplete parsing, traversal, symlink escape, or oversize
fails closed. Full content does not enter routine public diagnostics or metrics.

## Agent and skill defaults

Implementation, progress-creation, and refocus agents default to shared-parent sequential execution.
Completion, commit-message, and workflow-authoring agents default to fresh isolated sessions with
exact structured input.

Read-only workflow agents may attach a tool-using skill only when each required canonical operation
is read-only, available, profile-permitted, and node-allowlisted. Skills requiring mutation or
`disable-model-invocation` remain incompatible. Automated commit messages return typed title and
description directly to Git; they do not use editor-message-file behavior.

## Semantic graph editing and frontend adaptation

All editors modify `WorkflowAuthoringDocument`. A versioned semantic edit contract includes bounded
operations to add/update/remove nodes and edges; update schemas, predicates, transforms, bindings,
requirements, metadata, and namespaced presentation; and apply an atomic edit batch against an exact
draft generation. Unrestricted JSON Patch is not the primary contract.

A pure renderer-neutral reducer validates edit shape and produces a candidate document. The
application boundary authorizes and applies the batch, returning an updated draft, typed optimistic
conflict, or source-addressed diagnostics. It exposes catalog, draft, validation, preview, publish,
activate, and start operations without store rows or daemon-private objects.

The TUI owns terminal canvas layout, viewport, hit testing, input mapping, and drawing. Web and desktop
clients may present native canvases. Positions, groups, comments, and hints remain bounded presentation
metadata excluded from executable identity.

Prompt generation consumes only the portable catalog, emits the standard document, and receives the
same diagnostics. It gets at most three automatic repair attempts. The candidate must be shown and
explicitly accepted before creating or replacing a draft with optimistic generation. Generation
cannot publish, activate, start, grant permission, persist secrets, invent contracts, access private
APIs, or resolve conflicts automatically.

## Confined external template sources

Existing inline manifest templates remain compatible. A new versioned contribution may reference a
standard `WorkflowAuthoringDocument` beneath the plugin package plus its expected SHA-256. Discovery
canonicalizes and confines the path, reads it boundedly, verifies the digest, validates through the
standard authoring pipeline, and exposes normalized diagnostics. Instantiation creates a normal
mutable authored draft; later plugin updates do not rewrite it.

## Flagship state and composition

The workflow plugin owns `CodingWorkflowState`, including objective, prompts, completion condition,
progress reference, exact plans, instruction fingerprint, path policy, current product phase, latest
bounded summaries and receipts, and artifact references. Generic runtime counters remain outside it.

The implementation-batch child composes implementation, repository snapshots, validation,
formatting, post-format validation, exact checkpointing, and independent completion evaluation. It
returns completed or exhausted after at most 20 iterations.

The delivery-tranche child calls up to five exact batch revisions and performs approved refocus after
each exhaustion. The parent creates or attaches progress state, reviews tranche plan grants, calls up
to ten exact tranche revisions, and reaches a durable operator input wait between tranches. The full
hierarchy creates at most 60 descendants, under the root limit of 64.

## Mechanical acceptance boundaries

Architecture checks must enforce at least:

* coding-product types and integration names do not enter generic workflow packages;
* generic workflow packages do not execute shell, Git, progress-file, or project-instruction I/O;
* state-envelope authorization uses owner-operation value, not retained state;
* only workflow-store owns terminal-output and parent/child tables;
* workflow calls use exact immutable targets and bounded dependency admission;
* repository snapshot and checkpoint behavior remain Git-owned;
* progress mutation remains plugin-owned and approval-gated;
* authoring editors use portable contracts and no frontend accesses workflow-store internals; and
* external template paths are canonicalized, confined, bounded, and digest-verified.
