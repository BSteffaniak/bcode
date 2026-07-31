# Runtime workflow authoring architecture

## Purpose

Runtime workflow authoring lets a human, CLI, SDK, frontend, plugin, or generated producer describe a
workflow while Bcode is running. Every producer uses the same versioned, serializable application
contract and receives the same structured validation result. A renderer or generator may improve how
that contract is created, but neither owns workflow semantics.

This architecture extends the existing workflow domain. It does not introduce another workflow
engine, scheduler, canonical store, or frontend-specific definition format.

## Ownership and package boundary

`bcode_workflow` owns the portable authoring document, authoring identities, validation diagnostics,
normalization, compilation contracts, and executable `WorkflowDefinition`. The package remains free
of TUI, web, desktop, daemon-host, persistence, database, provider implementation, and plugin
implementation types.

`bcode_workflow_store` owns durable logical workflows, drafts, revisions, active-revision pointers,
presets, publication events, and links to compiled definitions in the existing canonical workflow
database. It must not define a second authoring model or compile workflow semantics independently.

The application/server boundary resolves catalogs, applies authorization, coordinates validation and
publication, and starts exact published revisions. IPC and clients serialize the workflow-owned
portable contracts; they do not expose store rows, daemon internals, provider-private values, or
renderer models.

`WorkflowApplicationOperationFacts` is the versioned workflow-owned policy input for side-effecting
authoring operations. It carries the exact operation; application-authenticated actor; workflow,
draft, revision, and preset identities applicable to that operation; untrusted producer provenance;
resolved requirements; aggregate effects and resources; and activation/execution intent. The daemon
derives local-client actor identity from the accepted connection and must not accept actor identity
from authored content. Tool-call/session permission facts are not substitutes for these application
facts.

`bcode_workflow` also owns portable authored-list and revision cursor contracts. Persistence validates
those cursors before using them for stable keyset queries; IPC, clients, CLI, and future frontends
share the same cursor representation rather than defining transport- or presentation-specific
continuation tokens.

Plugins continue to own contributed block behavior and plugin templates. A contributed template is a
versioned plugin manifest contract, not a user-owned mutable draft. A template may be used as source
material for a draft only through an explicit conversion or fork operation that produces a complete
portable authoring document.

No new crate is justified for this capability. The workflow domain already owns `ValueSchema`, graph
semantics, transforms, predicates, production admission, and executable definitions. Splitting the
new authoring types into a speculative package would either duplicate those contracts or point a
portable model back toward its implementation. Implementation therefore extends the existing
workflow-owned packages. If later dependency pressure proves that a lightweight leaf is necessary,
that extraction requires implemented consumers and a domain-specific package boundary rather than a
generic shared crate.

## Source, compiled, and runtime contracts

The lifecycle has three deliberately separate contracts:

1. `WorkflowAuthoringDocument` is portable source. It contains a schema version, stable logical
   workflow identity, metadata, configuration schema and defaults, declarative graph, runtime-defined
   `ValueSchema` values, generic configuration bindings, requirements, run-limit policy, and optional
   non-semantic presentation metadata.
2. `WorkflowDefinition` is the normalized exact executable contract. Publishing compiles source into
   this existing type, validates it, runs production admission, and records its digest and capability
   version.
3. `WorkflowRun` is durable execution state pinned to one immutable published revision and one exact
   compiled definition.

Compilation reuses the existing graph validator, predicates, transforms, schemas, node kinds, and
production-capability admission. It must not maintain a parallel interpretation of graph behavior.
Validation and compilation are deterministic, bounded, and side-effect free: they perform no durable
mutation, plugin dispatch, model request, tool call, shell or Git operation, or network access.
Catalog resolution supplies normalized available contracts to compilation. It does not transfer
plugin instances or provider implementation details into the source document.

## Logical workflows, drafts, and revisions

A logical workflow has an opaque stable `workflow_id`, mutable display metadata, archived state, zero
or more drafts, monotonically numbered immutable published revisions, and an optional active-revision
pointer.

A draft is mutable source state identified by `draft_id` and owned by one logical workflow. It records
an optional base revision, a monotonic generation, canonical checksum, timestamps, and normalized
producer provenance. Every replacement or typed patch supplies the expected generation or checksum.
A stale update fails with a typed conflict; last-write-wins behavior is prohibited. Discarding a draft
does not affect published revisions or runs.

Publishing is one atomic application operation. It verifies the expected draft generation, validates
bounds and versions, normalizes the source, resolves exact contracts, compiles and admits the exact
definition, then persists the immutable revision, compiled definition link, publication event, and
optional active-pointer update. Failure leaves no partial revision, definition link, event, or active
pointer.

Published revision content never changes. Editing a published workflow forks a new draft and
publishing that draft creates the next revision. Archive prevents default new starts without deleting
history. Activation is a compare-and-set update of a convenience pointer, not a rewrite of a revision.

Only a published revision of a runtime-authored workflow can start. A start using the active pointer
resolves that pointer centrally and returns and persists the exact revision selected. Exact historical
revisions remain startable while retained and supported. Existing plugin-template or internal exact
definition registration remains a distinct defined application path; it does not create mutable
runtime-authored state or bypass the publication requirement for authored documents.

## Active-run pinning

A run records its exact authored `workflow_id` and revision plus the exact compiled definition
identity and digest. Activating or publishing another revision affects only later convenience starts.
It cannot change graph topology, schemas, configuration, permissions, resources, reconciliation,
limits, or presentation of an existing run.

“Modify on the fly” therefore means fork, edit, validate, publish a new immutable revision, optionally
activate it, and start new work. Migrating an active run to another revision is outside this
architecture and must not be inferred from snapshots, active pointers, matching node IDs, or a
frontend action.

## Schemas, bindings, and presentation

Runtime-defined schemas use one explicitly versioned supported JSON Schema dialect through
`ValueSchema`. Validation bounds document and schema bytes, nesting, properties, enums, and local
reference expansion. Remote or network-dependent references are rejected. Unknown dialects and
future versions fail closed with source-addressed diagnostics.

Generic configuration bindings target only declared fields in node configuration, agent selection,
skill selection, plugin-block defaults, permitted predicates/transforms, run limits, or initial input.
Bindings use the existing bounded transform language or a separately versioned bounded extension;
they never execute arbitrary code. The fully bound result is validated again as an exact definition.

Optional authoring presentation metadata is versioned, bounded, and namespaced. It may store graph
positions, grouping, comments, or editor hints. It is excluded from executable identity and cannot
affect topology, type checking, compilation, admission, authorization, dispatch, or persisted run
outcomes. A client can ignore an unknown presentation namespace and still understand the workflow.

## Producer-neutral authoring

UI, CLI, SDK, plugin, and generated producers discover the same bounded portable catalogs and submit
the same authoring document. Catalogs describe durable node kinds, block contracts, agent profiles,
skills, predicates, transforms, schema dialects, production capabilities, and limit bounds without
leaking implementation objects.

Structured diagnostics contain stable codes, severity, source-document paths, and bounded remediation
guidance. A form or graph editor may map paths to controls; an AI producer may revise source from the
same diagnostics. Neither receives privileged validation behavior.

Producer provenance is normalized diagnostic metadata such as `human`, `cli`, `frontend`, `sdk`,
`plugin`, or `generated`, with a bounded producer identifier and source revision. It cannot grant
permission, select dispatch, alter compilation, or make content trusted. Generated and imported
content passes the same bounds, validation, publication authorization, and runtime approval pipeline
as human-authored content.

## Authorization and execution safety

Creating or changing durable authoring state, publishing, activating, archiving, importing, and
starting are side-effecting application operations. Applicable policy decisions over normalized
operation facts complete before mutation. Publication facts include exact workflow, draft, and
revision identity; producer identity; referenced capabilities; aggregate effect classes; resources;
and whether activation or execution was requested. The daemon application-operation authorization
boundary is separate from tool-call/session permission coordination. Its default local policy admits
local-client operations while plugin and service actors fail closed unless explicitly registered or
configured. Every mutation handler must authorize before acquiring the workflow store for mutation.

Publishing a workflow containing mutating blocks does not authorize those mutations. Existing exact,
activation-scoped grants and approval-before-dispatch rules remain in force. Imports and generated
documents never carry trusted grants. Presentation metadata and producer labels never affect policy.

Saved presets bind an exact revision and carry their own optimistic generation. They may hold bounded,
validated non-secret configuration and permitted limit/workspace policy. Authored documents, presets,
and durable authored-run provenance reject sensitive credential fields and explicit `env`/`sshenv`
secret-reference objects before persistence. Those references remain request-scoped invocation inputs;
persisting any reference form requires a future explicit, versioned contract rather than inference.
Starting a
preset requires its exact generation and records the preset identity/generation, revision, final
configuration, and compiled definition in the public start result. Exact-revision and active-revision
starts use the same centralized resolver. Every start rechecks current catalog/production admission,
requires the configured definition identity to equal the immutable published identity, then delegates
to existing durable run admission and runtime scheduling.

Validation, preview, and publication compilation accept a bounded server-side deadline and a stable
caller operation identity. The daemon executes bounded pure computation away from the async request
loop, rejects duplicate live identities, and supports exact cancellation. Timeout or cancellation
removes the operation registration and produces a typed public error. Publication performs this
computation before application authorization and before acquiring the workflow store for mutation,
so cancellation cannot leave a partial revision or active pointer. Client transport timeout remains
a separate observation deadline and cannot imply durable cancellation; callers that need explicit
server cancellation use the operation identity.

The IPC request loop treats validation diagnostics, unsupported future source, optimistic conflicts,
invalid computation controls, and publication conflicts as request-scoped outcomes. Focused real
connection tests send successful requests after each class of failure, proving these outcomes neither
poison nor close the local application connection.

## Import and export

Export uses a canonical versioned bundle containing an exact authored revision, schemas, bindings,
requirements, safe provenance, and optionally revision-bound presets. It excludes grants, secrets,
provider-private metadata, runtime receipts, attempts, artifact contents, and renderer-private state.
Export is a read-only bounded operation.

Import preview is side-effect free and runs the normal version, bounds, normalization, catalog,
validation, compilation, and production-admission pipeline. Import requests carry an explicit
collision policy: new-workflow import requires `require_new_workflow`, while existing-workflow draft
import requires `require_existing_workflow_new_draft`. A mismatched policy fails before mutation.
New-workflow import requires an explicit absent target identity; existing-workflow import requires an
explicit new draft identity and never rewrites revisions or active pointers. Semantic round-trip tests
export an immutable revision, preview it under a new logical identity, and prove the imported
executable projection changes only that explicitly selected identity. Imported provenance is
normalized to untrusted generated content and records the exact source revision.

Exact-revision import is distinct from draft import. It requires the target workflow to exist, an
explicit revision equal to the canonical next revision, the `require_existing_workflow_next_revision`
collision policy, and (when activating) the expected current active pointer. The standard preview and
authorization boundaries run before one atomic transaction persists the compiled definition,
immutable revision, optional active pointer, and `revision_imported` event. It never creates or
rewrites a draft, skips a revision number, or overwrites history.

Existing-workflow import is a distinct versioned operation from new-workflow import. It requires an
explicit target logical workflow and new draft identity, normalizes source provenance to untrusted
generated content, validates and compiles through the standard import-preview pipeline, authorizes
`ImportDraft` before mutation, and creates only a generation-1 mutable draft. A draft identity
collision returns a typed `DraftAlreadyExists` outcome and never overwrites or treats an existing
draft as an idempotent success. Existing revisions and the active pointer are unchanged.

Every public and persisted authoring form has an explicit schema version. Unsupported future versions,
unknown required variants, dialects, binding operations, or capability versions are rejected or
surfaced as incompatible; they are never guessed to mean an older form. Unknown optional presentation
namespaces may be preserved or ignored only because they are explicitly non-semantic. An export bundle
retains its declared version and cannot be relabeled during import.

## Producer workflows for AI and UI clients

All producers submit the same `WorkflowAuthoringDocument`; producer provenance is diagnostic and
never changes compilation or authorization. SDK, plugin, CLI, frontend, and generated producers use
the same lifecycle:

1. Read the bounded authoring catalog and construct a version-current document from its normalized
   node, block, agent, skill, schema, binding, and capability contracts.
2. Call validation and compilation preview without mutation. Treat each structured diagnostic code,
   document path, message, and remediation as data; an AI repair loop edits only the addressed source
   and repeats until the report is valid, while a UI maps document paths to form or graph controls.
3. Create or optimistically update a draft using its exact generation. On conflict, fetch the current
   draft and require an explicit user/producer merge rather than silently overwriting it.
4. Publish an exact validated generation, optionally compare-and-set the active revision, then use
   immutable revision inspection to display current requirement availability separately from
   publication facts.
5. Start an exact revision, active revision, or exact preset generation through the daemon boundary;
   renderer state and producer labels never affect authorization or dispatch.

Generated and plugin producers remain untrusted even when they reproduce byte-equivalent executable
semantics from an SDK document. They cannot carry grants, bypass application authorization, or turn
provenance into actor identity.

## Persistence, reads, and maintenance

Explicit authored maintenance is separate from every list/get/inspect/start path. The store operation
acquires an exclusive `SQLite` transaction, requires a new backup path confined to the canonical
workflow directory, creates a complete online backup, verifies its integrity and schema contract, and
only then mutates state. Repair is deliberately limited to disposable authored indexes and clearing
an active pointer whose target revision is provably absent. Missing compiled definitions, stale draft
bases, orphaned presets, and any other ambiguous canonical intent remain diagnosed for operator
resolution. A retained backup is never overwritten.

Authored lifecycle observability uses bounded dimensions only. Validation and compilation record
valid/invalid or compiled/rejected outcomes; publication records published/conflict; conflict counts
use a fixed operation vocabulary; import preview records accepted/rejected; and start resolution
records only the selection kind (`revision`, `active`, or `preset`). Metrics never label workflow,
draft, revision, preset, document, schema, prompt, secret, producer, or generated-content values.

Authored workflow inspection is one bounded, non-mutating aggregate query over indexed canonical rows.
Its public contract deliberately uses content-minimized draft, revision, and preset summaries plus
normalized publication-event fields. It omits authoring documents, schemas, node configuration,
preset configuration, prompts, secret material, generated prose, producer payloads, and arbitrary
event JSON. The architecture check mechanically requires these portable summary types and rejects
content-bearing or persistence-owned aggregate diagnostics.

It returns logical metadata and active pointer, bounded drafts, immutable revisions, presets,
publication events, and normalized consistency issues. The diagnostic query can surface invalid
active pointers, missing compiled definitions, orphaned presets, and drafts whose base revision is
missing; it never repairs, replays, rewrites, or treats derived availability as canonical state.
Generation conflicts remain typed mutation outcomes and are not inferred by read paths.

Canonical authoring state lives beside durable execution state in
`<state-dir>/workflows/workflow.db`. Authored starts atomically record schema-versioned provenance on
`workflow_runs`: the exact logical workflow, immutable revision, compiled definition identity,
optional exact preset generation, and resolved validated configuration. This metadata is diagnostic
only; authorization and dispatch continue to use normalized operation facts and the compiled
runtime definition. Run creation fails closed when the provenance does not match canonical revision
or preset rows, and caller-stable run identity retries must match the complete provenance.

Normal create/update/publish transactions may mutate only after
authorization. Normal get/list/validate/preview/status paths are bounded; read-only paths do not repair,
reindex, activate, publish, or dispatch work.

Validation reports and catalog projections are derived and disposable. Current plugin availability is
reported separately from immutable publication facts. Missing compiled definitions, invalid active
pointers, or inconsistent revision links surface degraded or repair-required state. Reconstruction,
forced pointer changes, destructive cleanup, and migration are explicit maintenance operations with
ownership and backup requirements.

## Public application operations

Portable typed operations cover:

* bounded catalog discovery and exact contract description;
* logical workflow create, get/list, archive, and unarchive;
* draft create/fork, get/list, optimistic update, validate, preview, publish, and discard;
* immutable revision get/list and active-pointer compare-and-set;
* revision-bound preset create, get/list, optimistic update, validate, and delete;
* exact export, side-effect-free import preview, and authorized import; and
* exact-revision, active-revision, exact-preset, and publish-then-start execution.

The routed mutation surface atomically creates a logical workflow with its generation-1 draft,
replaces or discards an exact draft generation, publishes an immutable revision with optional atomic
activation, compare-and-sets an existing revision as active, and archives/unarchives logical
workflows. Each derives the actor from the local connection and authorizes before locking the store
for mutation. Stale optimistic operations return typed conflict results carrying expected and current
values; they are not transport failures and leave the connection usable. Exact draft/revision forks
and revision-bound preset create/update/delete operations use the same authorization boundary;
preset updates and deletes retain generation conflict semantics and cannot change revision binding.

Immutable revision inspection returns publication facts and current requirement availability as
separate fields. The availability report is a versioned, bounded, renderer-neutral derived value
containing only normalized missing capability, plugin, block, agent-profile, and skill identities.
It is recalculated from the current catalog on each bounded inspection and is never persisted into or
used to rewrite the immutable revision. A host catalog change may therefore degrade the report while
leaving publication facts byte-for-byte unchanged.

Executable authoring identity is derived from the explicit portable
`WorkflowExecutableAuthoringSemantics` projection. That projection contains configuration schemas and
defaults, graph semantics, bindings, requirements, and run limits, while omitting user-facing
metadata, producer provenance, and presentation payloads by construction. Tests change all omitted
fields simultaneously and require both the executable digest and complete compiled preview to remain
identical; the workflow architecture check requires this projection and regression coverage to stay
present.

Authored starts resolve exact revision, active revision, and exact preset generation through one
daemon application function. It reads only immutable published revision rows; explicit older
revisions remain startable while retained, and stale preset generations fail before admission. The
selected immutable document is recompiled against the current host catalog and resolved
configuration before authorization and run creation. Configuration-schema failures and unavailable
required plugins, blocks, agents, skills, capabilities, or schema versions therefore fail closed
without mutating publication state.

Publish-and-start is one versioned application operation but preserves two durable outcomes. The
publication result is either a typed optimistic conflict or a committed immutable revision with its
active pointer result. Only after committed publication does the daemon attempt run admission; that
second result is returned as either the exact authored-run start response or a structured public
error. A failed run admission cannot appear to undo publication, and retry-safe caller run identities
retain the normal complete-provenance conflict checks.

## Mechanical enforcement

`scripts/check-workflow-architecture.sh` enforces that:

* `bcode_workflow` does not depend on frontend, renderer, daemon, database, persistence, provider
  implementation, or plugin implementation packages;
* workflow-owned source does not import known terminal, web, daemon, database, provider-private, or
  plugin-runtime implementation types;
* only `bcode_workflow_store` owns the canonical workflow database path and authoring tables; and
* durable registration/start continues to use production capability admission.

Focused model, persistence, IPC, and integration tests will enforce version rejection, canonical
identity, presentation neutrality, optimistic conflicts, atomic publication, active-run pinning, and
producer-neutral behavior as those contracts are implemented.
