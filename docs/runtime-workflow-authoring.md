# Runtime workflow authoring architecture

## Explicit terminal-outcome collection

V2 task groups may select `failure_policy: "collect_outcomes"` for two or more
independent workers. The continuation receives `results[task_id]` as either
`{"status":"completed","value":...}` or `{"status":"failed"|"cancelled"|"skipped"}`.
Successful values remain schema-validated canonical outputs; failures do not gain
invented outputs. Receipt assignments mark `terminal_outcome: true`. The source
context join remains success-only and source preservation is unchanged.

The durable controller waits for all declared members, preserves their terminal
states, and materializes one named outcome input. It does not retry effects or
assert goal completion. The continuation must inspect evidence and unresolved
effects before authoring corrective work. Existing `wait_all`/`fail_fast` semantics
are unchanged. Dependent-worker groups and homogeneous fan-out do not support this
mode and reject it. Old runtimes reject the unknown policy rather than interpreting
it as success-only behavior. Live-model recovery remains unverified.

## Source-preserving integration

Compact v2 staging receipts retain publication arguments and named result paths,
but omit the redundant reconnect transform/schema. Corrective authors inspect the
current revision-pinned successor through `workflow.execution_context`; a staging
receipt is neither current topology nor permission to infer an absent transform.

`workflow.execution_context` now returns a `delegation` recipe for a source with
one direct successor when the bounded response contains its definition and all
edges from the initial edge cursor. This works with compact presentation too:
the plugin uses the authenticated typed edge, including its actual transform.
Copy `delegation.arguments`, add `mutation_id`, semantic `tasks` and a
`continuation` objective/profile, then call `workflow.stage_task_group`. Workspace
selection, access, criteria, dependencies and integration strategy remain authored.
The recipe preserves canonical source output and supplies revision, allocation,
source activation binding and reconnection without model reconstruction. It grants
no authority: staging still validates reconciliation and publication remains
separate. Conflicts require fresh discovery. Partial pages, unavailable allocation,
multiple/control successors and unsupported transforms return an actionable
unavailable result; use advanced staging for those topologies. This bounded helper
is not automatic planning, workspace integration or evidence of worker success.

For oversized recipe schemas, request `delegation_part: "serialized"`. Concatenate
returned `delegation.chunk` strings in order, following `next_arguments` unchanged
until null, then parse the concatenation as the exact staging arguments. Each
chunk is bounded independently of schema size; offsets count Unicode characters,
not bytes. Later chunks require the pinned revision. Restart on revision conflict;
never combine chunks from different revisions. This is read-only transport of the
same authenticated recipe, not a schema simplification or additional authority.

Task-group workers and continuations may select `worktree_directory`, an absolute
path to an existing registered worktree of the parent session's repository. Prompt
configuration version 5 persists this choice; versions 2–4 upgrade only with the
inherited workspace and reject an explicit worktree field. Fresh and fixed-generation
sessions use the selected worktree; shared-parent execution rejects relocation.
The host validates registration before session creation and checks recovered session
workspace identity before continuing. Missing or foreign worktrees fail closed.
This does not create a sandbox, grant tool permissions, create/clean up worktrees,
or integrate contributions. Use ordinary authorized worktree operations first,
explicit scheduler claims for shared locations, and inspect/verify integration.
Omitting the field retains existing parent-workspace behavior. Assignment context
includes the declared worktree but is not proof of contribution correctness.

Workspace creation with `base_ref: "head"` resolves the invoking checkout's exact
commit, including detached linked worktrees, before creation. `detach` changes
branch ownership, not base selection. Unresolvable HEAD fails closed. This does
not copy uncommitted work or pin later worker edits; authors still own contribution
provenance and integration verification. Creation responses now include optional
`provenance`: the canonical source checkout, full created `base_commit` observed
before setup, and conservative `source_has_local_changes` (including ignored files
or failed inspection). Older responses omit it; absence means unknown. This is an
observation, not a source lock, copied dirty snapshot, or integration receipt.
Retain it with worker evidence alongside produced revisions and observed checks.
For a dirty source, integrate in a separate registered target first; inspect and
resolve conflicts there, verify the combined artifact, and preserve the user's
uncommitted files. Do not automatically stash, reset, or force-clean the source.

Workspace cleanup uses the existing worktree service. Non-forced removal refuses
uncommitted, ignored, or unverifiable files, regardless of Git's untracked-file
display configuration. Detached contributions must first be retained by a branch,
tag, or remote-tracking ref containing HEAD. Keeping a ref protects the contribution;
it does not prove integration or verification. Explicit force remains destructive
and must not be used as automatic cleanup for ambiguous worker outcomes.

Task-group continuations and v2 result receipts receive the same normalized worker assignments: objectives, criteria, output schemas, agent profile, model selection, tool allowlist, timeout, context, workspace, read-only access, resource claims and dependency task IDs. Omitted model/timeout values remain null (normal resolution/defaults), not observed execution facts. These assignments help interpret contributions; they are not execution evidence. Resource claims coordinate scheduling and fresh contexts do not isolate files. Integration must inspect actual workspace state and independently verify the combined result.

Task-group v2 accepts explicit `preserve_source_output: true` with
`reconnect`. Omitted `include_source_output` derives `true` in this mode; explicit
`false` rejects rather than weakening preservation. `continuation.output` must equal
the source input schema.
Omit `continuation.output` in this mode to derive it deterministically from `input`;
explicit null or incompatible schemas still reject. Other modes require an output.
Lowering uses the existing host `PreserveInput` prompt policy and a typed reconnect
transform selecting `source` from the named envelope. Integration can perform
authorized work and verification without recreating protected goal state. The
downstream evaluator, not continuation settlement, determines completion. Corrective
delegation may copy an existing current-input source-only selection into
`reconnect.transform`; lowering prefixes that selection for the new envelope.
Unsupported transforms fail closed rather than being silently overwritten.

V2 accepts `worker_defaults` for repeated `agent_profile`, `context`,
`model_selection`, `timeout_ms`, shared `acceptance_criteria` and a typed `output`
schema. Use `worker_defaults.output` to declare a common contribution/evidence contract
once; omitted worker outputs inherit it, while explicit schemas override it. Explicit
null rejects rather than selecting the bounded fallback. A schema describes evidence,
not proof of success. Explicit worker fields override by presence,
not truthiness; normal typed validation still applies. An explicit criteria array
replaces, rather than appends to, the defaults (including an empty array). Defaults
never apply to integration. Access, resource claims, tools, identities and objectives
remain task-specific; unsupported default keys and v1 defaults reject. Expansion
is deterministic before staging and exact-request publication replay.

V2 also accepts opt-in `generated_ids: true`: omit `join_id` and
`continuation.task_id`. Lowering derives those mechanical identities from the exact
UTF-8 mutation identity using hexadecimal encoding; explicit worker IDs remain the
semantic result keys. V2 staging receipts expose generated identities in `node_ids`
and omit the redundant expanded `edit`; legacy task receipts retain `edit`.
V2 receipts also include `result_mapping`: the normalized `continuation_id`, ordered
`workers` with exact `task_id`, `input_path` key arrays, normalized `output` schemas
and `acceptance_criteria`, plus requested `context`, `worktree_directory`, `read_only`,
normalized `resources` and `depends_on`; optional `source_path`
(null when absent), and `preserves_source_output`. `reconnect` is null without a
successor; otherwise it contains the exact lowered edge ID, successor node ID and
transform, including composed source preservation. The same successor is supplied
to the integrator's prompt. Before corrective reuse, verify it against a revision-pinned
execution-context read: it describes the authored candidate, not current graph state.
This avoids rebuilding nested source selectors without hiding reconciliation or
publication authorization. Key arrays avoid ambiguous dotted
paths for Unicode or punctuation in worker IDs. Continuation assignment metadata uses
these same `input_path` arrays in both positional v1 and named v2 groups: string
segments are literal keys and integer segments are array indices from the entire
continuation input. Missing paths are missing evidence, not successful empty results.
Workers using the exact bundled result schema also receive `evidence_paths` for
`summary`, `evidence` and `blockers`, in both receipts and continuation assignments.
Custom schemas (even with the bundled type name) are not interpreted. Nonempty
blockers require inspection and correction or an actionable blocker; empty blockers
are not proof of integrated verification.
This derived description is neither
canonical execution state nor evidence that any worker succeeded. Receipt mapping and
candidate lowering share one normalized request; mapping is returned only after its
candidate matches the application-admitted edit, including topology validation.
Compact v2 receipts retain worker identities and result/evidence paths, but do not
repeat worker prompts, criteria or output schemas. Those assignment details remain
in the authored request and canonical nodes; large assignments must not obscure the
publication reference. Large worker counts can still require retained receipt inspection.
Both permission preparation and invocation perform the same deterministic lowering.
Task staging receipts include bounded `publication_arguments` containing a versioned
candidate reference (run, mutation, revision and SHA-256 of the complete serialized edit).
The application authenticates the active execution, resolves the retained candidate,
checks the complete binding, then authorizes publication using the full canonical edit.
Pass those arguments to
`workflow.publish_run_graph_edit` under separate authorization rather than rebuilding
the graph edit. This is explanatory output, not a publication or authority token.
Identical retries must retain the original request and mutation identity; collisions
still reject canonically. Legacy explicit-ID requests remain supported. Neither
convenience grants execution authority or establishes integrated verification evidence.

Corrective reconnection after a repeat may target a node whose older generations
have already settled. Publication requires the replaced edge's explicitly retained
active source to establish the later dependency generation, and rejects any active,
unknown, same-generation or newer target admission. Historical outcomes remain
unchanged; this permits future evaluation, not replay of a completed activation.

## Revision-pinned edge allocation advice

Graph inspection, including authenticated `workflow.execution_context`, exposes
`graph.next_edge_id`: the first edge identity above all identities ever used by
the run, including retired edges outside the bounded page. The indexed lookup
shares the page snapshot and does not scan or reserve the graph. Coordinators
use it as task-group `first_edge_id` with the observed revision; concurrent
publication still conflicts normally. Absence means unknown (older sender) or
identity-space exhaustion, not permission to guess from a partial page. Range
overflow and collisions still reject during canonical edit validation. This
removes edge-watermark bookkeeping, not node naming, explicit reconciliation,
or the current multi-page publication limitation.

## Delegation prerequisite inspection

The typed client `workflow_delegation_preflight(plugin_id)` reads the matching daemon's loaded
plugin state and explicit staging/publication configuration grants. Its portable v1 response
is advisory, not an execution grant or a promise that a future candidate will be authorized.
Unknown plugins report unloaded; invalid identities reject. Workflow-domain unavailability
fails through the normal application boundary. No configuration is modified. The loop plugin's
`/goal.preflight` command consumes this operation and reports missing plugin/staging/publication
configuration with explicit remedies. It rejects unsupported response versions and mismatched
plugin identities without interpreting their flags. Missing prerequisites return command failure
alongside the remedies; success means only that these configured prerequisites are present.
It does not start work or verify model/workspace readiness.
Collaboration setup builds and dispatches a coordinator
configuration with a fresh execution context, input-preserving output and bundled plugin-owned
coordination instructions. Its portable activity stage is `coordination` (and `coordination_complete`
on settlement); fallback text explicitly does not establish worker dispatch/results/integration and
points to `/workflow`. The goal graph and evaluator policy/schema remain unchanged; collaborating
evaluation adds plugin-owned instructions requiring canonical delegation/integration evidence and
an incomplete verdict when evidence is missing. Optional judgement remains unchanged. This
configuration is dispatched after prerequisite success through ordinary start authorization;
handoff, allowance accounting and end-to-end execution still need acceptance proof. Fresh context is not filesystem isolation, and the unchanged mutating
capability declaration grants no tools or publication authority. `/goal --collaborate` explicitly selects
collaboration and checks prerequisites before fresh-session creation and prompt generation,
then checks again before working-document creation, replacement cancellation or workflow start.
Generation receives explicit collaboration guidance without granting tools or prescribing a strategy.
Pending generation checks coalesce; closure discards late results, and failures permit retry.
Missing/incompatible prerequisites block with remedies. Successful launch preflight retains the
exact coordinator request through document preparation and starts it once; an explicit retry checks
prerequisites again. A denied replacement re-enters replacement confirmation instead of becoming
a direct-start retry; a denied direct start retains its exact request identity. Ordinary `/goal`
implementation receives optional coordination guidance and a fresh workflow execution context,
with or without progress notes. It may still work directly; fresh context is not workspace
isolation. Existing admitted definitions are not rewritten. Unknown goal options
reject with usage. These checks grant no authority and do not verify model/workspace readiness.
`/goal` setup offers
Ctrl-D for a nonblocking advisory check through the host API; repeated pending checks coalesce.
The setup check times out after ten seconds and permits retry without treating timeout as readiness.
Results do not start generation or grant authority. The full multiline remedy remains available
through `/goal.preflight`; setup shows a single-line prerequisite summary pointing to that command.
Plugin surfaces can use
`PluginTuiHost::workflow_delegation_preflight` asynchronously; the terminal host delegates to its
existing typed client and unsupported hosts fail closed. This adaptation owns no permission policy.

## Continuing revised goal loops

Run inspection includes an optional `execution_allowance` observation with run/root caps and
consumed attempts (counts saturate at each cap). Counts are nullable: inspection scans at most
1,001 local attempts, 1,001 descendant links, and 1,001 root/descendant attempts; if those
read bounds prevent a trustworthy count it reports unknown, never zero or remaining capacity.
These are inspection bounds, not execution or composition limits. Older senders omit the observation, meaning unknown.
The shared `exhausted()` query returns true if either valid budget proves exhaustion, false only
when both known counts are below their caps, and unknown for incomplete or inconsistent facts.
The workflow plugin's `workflow.inspect` structured result includes the observation, and its
summary distinguishes exhausted, remaining and unknown allowance using the shared query.
Neither remaining allowance nor the summary grants dispatch authority.
The read uses a single database snapshot and does not mutate state; it does not imply runnable
work or authorize any increase. Historical exhaustion events are not used to derive these facts.

`/goal --worker-attempts N` accepts an explicit positive extra execution allowance
without requiring collaboration; `--collaborate` additionally requests collaboration.
Request construction adds it to the base goal-node allowance with checked arithmetic;
it does not change goal rounds, concurrency/recursion policy or permission authority. It is a
run-wide extra attempt budget (workers and added coordinators), not a worker count or reserved
per-role quota. Omission adds no budget. Plain `/loop` behavior is unchanged.
Full collaboration acceptance remains unverified. `/goal.continue R --worker-attempts N` explicitly adds
N attempts to the retained graph's renewed base allowance without changing its input, graph,
round count or recursive policy. Successor concurrency is the smaller of the retained concurrency
cap and the renewed total attempt allowance, preventing invalid limits on short continuations.
Omission adds no extra attempts; overflow and
combined allowances outside the store's signed 64-bit integer range reject before launch effects.
This is a new caller-authorized budget, not automatic renewal of the previous extra allowance.
Mid-round delegated exhaustion and end-to-end continuation acceptance remain unverified.
The store provides an application-authorized, execution-fenced compare-and-set increase of
one active run's attempt cap. It preserves attempts, graph, outcomes and other limits;
child increases do not increase root allowance. Exact expected/target retries are no-ops
while the target remains current and the latest bounded grant event matches the exact cap pair;
missing, malformed or conflicting grant evidence and intervening caps reject. Successful changes atomically
record a version-1 `execution_allowance_increased` event with `previous_cap` and `target_cap`.
Idempotent run admission compares the original attempt cap from the first bounded grant
event, not the renewed execution cap; invalid or unsupported admission evidence rejects.
Public history exposes only those reviewed fields, requires positive increasing signed-range
caps and version 1, and reports unavailable details for malformed or unsupported observations.
This storage capability is not yet exposed through public authorization/control or `/goal`
commands and does not itself resume dispatch. Existing
continuation requests for non-eligible run status or failure reason return the normalized
`workflow_continuation_ineligible` error rather than generic workflow unavailability.
This rejection preserves the run and explicitly reports that active allowance exhaustion is not
yet supported. These options alone do not deliver swarm execution. Dispatch reports run/root execution-allowance
exhaustion separately from concurrency contention and leaves blocked activations pending without
admitting attempts. This allows the driver to reconcile already-admitted receipts despite exhausted
allowance. Settlement does not replenish execution allowance; no automatic grant or successor
continuation is implied. The first blocked dispatch records a version-1
`execution_allowance_exhausted` history event under the observed execution authority. Its
`additional_work_admitted: false` payload describes that observation, not a terminal outcome
or a current-status projection. Repeated blocked passes do not append duplicate observations;
existing attempts, results, run status and allowances are unchanged. Public history exposes only
these reviewed version-1 fields, drops additional producer data, and reports unavailable details
for missing/unsupported versions or malformed observations.

The loop plugin derives an exhausted loop's renewed node-execution allowance from reachable
Task, Agent, PluginBlock and WorkflowCall nodes in the retained graph, multiplied by the requested
additional rounds and retry allowance. It no longer assumes exactly two or three execution nodes.
Existing launch uses the same executable-node count, reserving initialization once and the
remaining executable nodes per round. Existing goal/loop topology checks still apply. This is a conservative per-round graph count, not
separate worker budgeting or proof of arbitrary nested-control continuation.

## Cancellation continuation

Periodic keyset discovery now considers paused/repair-required runs with their own
or direct parent's durable cancellation flag, in addition to running work. The
child driver inherits intent under child authority before scheduling. Run-wide
cancellation signalling is paged by dispatch identity alongside publication
cancellation; missing owners remain unresolved and are retried on later drives.
No terminal outcome is inferred from signalling failure. This uses existing durable
flags, not an in-memory traversal as the source of cancellation authority. Full
multi-level restart and foreign-owner integration acceptance remains unverified.

## Published workflow calls

Connected publication now permits pre-existing unchanged controller nodes and unchanged
non-direct/transformed edges whose endpoint executables remain identical. This allows a
candidate to coexist with an existing repeat loop without authorizing controller rewrites.
New, changed or deleted controllers/control edges still fail this publication proof; existing
controller incident edges must remain identical, including no added incident edges. Validation coverage
includes an unchanged repeat definition; full goal-loop delegation execution remains unverified.

Connected publication permits typed transforms on direct edges using canonical transform
validation and runtime evaluation. This is required by v2 named task-group results and
source-preserving corrective delegation. Controller incident edges and non-direct edges
remain unchanged; transformed dependencies do not bypass admitted-target reconciliation.
A goal-generation → plugin-lowering → store regression covers two workers followed by a
second corrective group and verifies the original evaluator input. Agent outputs and host
admission are simulated; daemon, permission, live-model and filesystem acceptance remain open.

Connected graph publication now permits `WorkflowCall` nodes through the existing
child dispatcher. Added/replaced call nodes must resolve an available exact target
and match its input/output interfaces before publication commits; this validation
also applies to disconnected leaf additions. Non-identical mapped interfaces are
not supported by this publication proof. Publication does not override child
admission policy: inherited authorization, workspace restrictions, recursion checks,
and execution allowances still apply. Active-planner suspend/join and policy-governed
recursive reuse remain unfinished; permitting call nodes does not establish them.

## Agent task staging

Parallel result composition now expands supported nonrecursive local schema references within
each component's own root before embedding. This preserves independent `$defs` namespaces and
reference siblings, including goal judgement configuration, under an explicit expansion budget.
Task-group worker and source-context joins share the workflow-domain helper. Unsupported recursive
references, resource identifiers and dynamic references reject rather than weakening constraints.
Existing persisted schemas are not rewritten or repaired by this change.

Task-group lowering validates worker and integrator prompt configurations before producing
permission-preparation candidates. Invalid worktree paths, shared-parent relocation, zero
timeouts and blank tool entries reject before staging; registered-worktree membership and
execution authority remain application-time checks.

Task groups accept optional `dependencies`, mapping a worker task ID to one predecessor worker
in the same group. Dependent workers consume predecessor output; only roots consume group/source
input and become entries when no external source is selected. Cycles and unknown workers reject.
Aggregation still includes every worker in request order. This lowers to canonical direct edges,
not a second scheduler; publication/admission and execution acceptance remain separate.

Task-group workers and `workflow.stage_prompt_task` may omit `output` to use `bcode.delegated_task_result.v2`: an object
with required `summary` (up to 4096 characters), `evidence` and `blockers` (each up to
32 strings of 2048 characters). Optional `contributions` holds up to 32 observed workspace
records: creation-time base revision, source directory and local-change observation (nullable
when unknown), produced revisions/artifacts, validation commands with `passed`, `failed` or
`not_run` outcomes and evidence, remaining work, and retention (`retained`, `removed`, `unknown`).
Omission means unknown, not clean or integrated. Records are worker claims, not authenticated
Git facts or approval to commit/clean up. Integrators receive literal contribution paths and
instructions to inspect actual changes, preserve user work, surface conflicts, verify the combined
target, and identify integrated artifacts and unresolved acceptance criteria in their final response.
This is evidence transport and integration guidance, not automatic Git integration or a verified
completion report. Existing persisted v1 schemas/outputs remain unchanged; explicitly authored
v1 contracts remain supported as custom schemas. New default lowering uses v2; retries of older
staged requests require their original compatible artifact or exact staged candidate, not silent
re-lowering across versions. Explicit schemas remain supported; null rejects.
Continuation output remains required to preserve caller-selected downstream contracts.
This default is result evidence, not an automatic completion verdict. Single prompt tasks also
accept optional `acceptance_criteria`, using the same evidence instructions as task groups.
Blank criteria reject; omission leaves the original objective unchanged. Single tasks also accept
positive `timeout_ms` and `tool_allowlist` restrictions, retaining canonical defaults when omitted.
These do not grant tools, raise the read-only ceiling, or extend run allowances. Optional
`model_selection` (`provider`, `model`) uses the same canonical configuration as task groups;
normal runtime model resolution and authorization still apply. Empty selections reject.
Single tasks also accept `context`: `fresh_isolated` (default), `fixed_generation_fork`, or
`shared_parent_sequential`. These reuse canonical context policies, not filesystem isolation;
read-only authority is unchanged and unknown/null policies reject. Optional `resources` passes
canonical read/write scheduler claims to the node. A write claim requests exclusive scheduling,
not mutating tool authority; single prompt tasks remain read-only. Omission declares no claims.
Single tasks may specify `depends_on: {node_id, edge_id}` with `entry:false` to add a direct
source dependency without raw edge JSON. The source must be distinct/nonblank; the caller supplies
an unused edge ID and matching input schema. Existing successors remain; publication still validates
topology and authorization. This is not a multi-result join or an automatic handoff.

`workflow.execution_context` also returns optional `execution_allowance` using the existing
bounded run/root observation under authenticated execution authority. Older senders may omit
it, meaning unknown; it neither reserves attempts nor grants budget increases. Older strict
consumers can reject the extended response and must use a compatible plugin artifact.

`edit_json` may also encode `{ "task_tool": "workflow.stage_task_group", "request": <original payload> }`
(or either single-task staging tool). This re-lowers the exact original request when the expanded
staging edit was truncated. It does not look up or publish by mutation ID alone: the server still
requires equality with the retained candidate and separate publication authorization. Changed
requests or incompatible lowering cannot silently replace the staged edit. Older plugin artifacts
reject this representation; callers must not assume support across versions.

Execution-context responses also include `next_page_arguments` (null when the
observed collections are exhausted) and per-output `inspection_arguments` for
checksum-verified value reads. Follow these recipes verbatim; independent cursors
are retained across empty collections and reads remain revision-pinned. A full
output page may require a final empty read. Outputs arriving behind a cursor
require a fresh scan; these recipes are not subscriptions, reservations, proof of
complete discovery from an arbitrary starting cursor, or authorization.

For coordination reads, `workflow.execution_context` accepts `compact:true`: the
plugin presents `graph.node_ids` and `node_definitions_omitted:true` instead of full
node definitions. It omits edge transforms (`edge_transforms_omitted:true`) and
lists output identity, node and checksum rather than values (`output_values_omitted:true`).
Authenticated identity, revision, allocation advice, edge endpoints and pagination
flags are unchanged. Each compact edge includes `inspection_arguments`: exact
revision-pinned, noncompact one-edge read arguments for `workflow.execution_context`.
Use these before copying a successor transform for corrective reconnection; omission
from compact presentation never means the edge has no transform. Revision conflicts
require fresh discovery, not guessed or remembered transforms. These read arguments
confer no publication authority. Read normal revision-pinned pages for executable definitions;
request an exact `output_id` to retrieve its value. Explicitly requested outputs are
not shortened by compact presentation. Large pages or requested outputs may still
exceed the model's tool-output budget; truncated results are not complete JSON.

For recipe-only reads, `delegation_only:true` excludes duplicated graph facts.
For larger corrective recipes, `delegation_part:"bindings"` returns non-schema
arguments and exact revision-pinned `inspection_arguments.input` and
`inspection_arguments.reconnect` requests. Follow both and merge their returned
`delegation.arguments` with the bindings; no schemas or transforms are reconstructed.
A revision conflict invalidates all pieces. These presentation options do not change
canonical discovery, preparation, authorization or topology validation. Unsupported
recipes still return an explicit reason. Individual very large schemas may still
need bounded retained-output inspection; splitting is not an unlimited-output guarantee.
The goal-entry scripted-provider tests exercise initial and corrective staging at
the default 4,000-character budget, not an enlarged test-only limit.

`workflow.execution_context` accepts `{}` for its initial page, defaulting to 50
items. Explicit limits remain 1–100; null, invalid limits and unknown fields reject.
The plugin normalizes this default during both preparation and invocation; the
shared application request still requires an explicit limit.

`workflow.stage_run_graph_edit`, `workflow.publish_run_graph_edit`, and
`workflow.accept_run_graph_publication` accept a structured object as `edit`, avoiding
manual JSON-string re-encoding. Publication/acceptance require the exact staged object.
Legacy `edit_json` remains supported; supplying both or unknown envelope fields
rejects. Canonical validation, preparation matching and separate publication authorization
remain unchanged. This does not automate coordinator handoff.

`workflow.stage_task_group` supports request versions 1 and 2; omission retains the initial v1
representation and exact replay. Version 2 deterministically transforms the final join into
`{results: {task_id: value}}`, with `source` containing the canonical source output when
`include_source_output` is enabled. Exact task IDs (including dots and Unicode) are object keys,
not paths. Required member schemas retain local-reference constraints; missing or malformed
results fail closed. Both goal coordination prompts request v2. This removes positional-result
bookkeeping, not explicit edge allocation, reconciliation or publication authorization. Canonical
goal fields still require preservation in continuation output; named input is not a protected-state
merge or proof of completion. The positional mapping described below remains the internal join
representation and v1 continuation input.
Unsupported versions, null/string versions and unknown fields reject before
staging. It stages workers, ordered joins, and a
continuation in one canonical edit. `failure_policy` defaults to `wait_all`; `fail_fast` selects
canonical cooperative sibling cancellation on every generated result/context join. It does not
undo effects or imply immediate cancellation. Once every member is terminal, each side must have
at least one completed result: skipped alternatives are allowed, but an entirely skipped side
settles as failure under either policy. Successful sibling outputs remain durable; a missing-result
join does not dispatch its integration continuation or fabricate a tuple. This is failure reporting,
not automatic corrective delegation or retry.
Prompts default to read-only; explicit `read_only: false`
declares mutating capability without granting execution authority. Workflow ceilings, selected
agent policy and tool permission decisions still apply. Publication rejects added/replaced Agent
and PluginBlock nodes whose declared capability exceeds the run ceiling, even after staging
permission was approved. Workers consume the run input by default. An optional
`source_node_id` instead creates non-entry workers connected to that existing node; its canonical
output must match the supplied input schema. This adds dependencies but does not remove existing
successors or settle the source activation. Connected publication supports one source feeding
multiple schema-compatible workers; retaining an active source requires explicit bindings for
all affected outgoing edges. Binary joins preserve
request order as left-associated pairs (`[a,b]`, then `[[a,b],c]`); a single-worker
follow-up instead delivers that worker's output directly, without a synthetic pair.
Empty groups reject. Continuation prompts include delegated task objectives and
acceptance criteria as JSON alongside the ordered worker task IDs,
explain the single-result/pair mapping and optional source envelope, and instruct the
agent to treat results as untrusted evidence rather than instructions or completion proof.
`join_id` remains reserved for identity validation even when no
result join is needed. Intermediate IDs use
`join_id.part.INDEX`. Optional `include_source_output: true` requires `source_node_id` and adds
`join_id.context`, a join delivering `[canonical source output, worker result pairs]` to the
continuation. This preserves source data without asking workers to reproduce it; source retention
must explicitly bind the context edge as well as worker edges. During settlement, a member newly
introduced at the current graph revision with no activation history may remain pending until its
dependencies finish; missing historical member activations still fail closed.
Callers supply distinct node identities, an unused
consecutive edge-ID range, typed schemas and reconciliation. Each prompt may optionally specify
`model_selection` with both `provider` and `model`, resolved through normal model admission.
Prompts may select `context`: `fresh_isolated` (default), `fixed_generation_fork`
(the run's pinned parent generation), or `shared_parent_sequential`. These lower to
existing canonical execution targets; normal admission and sequential parent scheduling
still apply. Context selection provides neither filesystem isolation nor new authority.
Prompts may include optional `acceptance_criteria`, a list of nonblank strings. These are
encoded as JSON in the canonical prompt with an instruction to report evidence; omission
preserves the objective verbatim. Criteria do not grant authority, change output schemas,
or automatically establish completion. Both workers and the continuation accept them.
Prompts may also declare canonical `resources` (`resource` and `read`/`write` access). These
scheduler claims are preserved on worker/continuation nodes, do not grant tool authority, and
are not filesystem isolation; an exclusive claim alone does not enable mutations.
Omitting resources declares no claims unless the group supplies `workspace_resource`.
This opt-in shared scheduler identity adds read claims for read-only workers and the
continuation, and write claims for mutating ones. Existing stronger claims are preserved.
Matching writers serialize against readers and writers through the canonical scheduler;
read-only workers can still overlap. Use the same identity for all cooperating work.
This is not filesystem isolation, path confinement, cross-run locking, or permission to
mutate. It does not protect against unrelated editors or resolve conflicting contributions;
isolated worktrees and explicit integration remain separate authorable choices.
Optional positive `timeout_ms` and `tool_allowlist` lower
to canonical prompt constraints; omission retains the prompt timeout and normal agent tool
selection. An allowlist never grants tool permission, and a task timeout never extends run limits.
Publication remains separate. This
adds independent entries only when no source is selected. Optional `reconnect: {edge_id, node_id}`
replaces the selected existing edge with continuation → successor and makes the continuation
non-exit. It requires a source selection and explicit active-work reconciliation; publication
validates the candidate against actual edges and schemas. Other successors remain unchanged.
For v2, optional `bind_source_activation` explicitly consents to retain that activation and
bind its output to every generated edge from `source_node_id`, including source-context
aggregation but excluding worker-to-worker dependencies. The plugin computes edge identities
in the exact canonical batch before permission preparation. Do not also list that activation
in `reconciliation`; conflicting dispositions are rejected. Other affected activations still
need explicit reconciliation. Optional v2 `retain_source_edge_ids` lists caller-selected existing
source bindings to combine with generated bindings. It requires `bind_source_activation` and
rejects duplicates, generated IDs and the replaced reconnect edge. Inspect every relevant
revision-pinned edge page: this list is explicit retention intent, not a claim that a partial
page is complete. Canonical staging validates the exact set against actual graph state.
Omitting the list preserves generated-only behavior. Omission of `bind_source_activation`
grants no bindings; v1 rejects this convenience.
Canonical staging still verifies source ownership and schema compatibility, and publication
remains separately authorized. This does not allocate graph identities or auto-select a successor.

An optional `reconnect.transform` carries a typed canonical `WorkflowTransform` onto the
replacement edge in either request version. Callers must explicitly retain any required
state-protection transform; omission keeps the legacy untransformed behavior. Exact replay
includes the transform, and normal publication validates its compatibility. This does not
infer the original edge or automatically protect goal state.
With v2 `preserve_source_output`, an existing current-input selection whose path contains
only `source` segments is composed beneath the new envelope's `source`. This allows corrective
delegation to retain the original goal state without model reconstruction. Copy the existing
transform exactly and use the current continuation's output schema as the next group's input
and declared continuation output. Other transforms and unknown versions reject in this mode.
This tool does not yield the current turn or wire
itself into the goal loop. Existing admission and run allowances remain authoritative.

`workflow.stage_prompt_task` provides a read-only prompt-task shorthand: callers supply a task ID,
objective, agent profile, typed input/output `ValueSchema`s, entry/exit flags, optional edges and
explicit reconciliation. The plugin lowers this to the same canonical staging request used by
`workflow.stage_agent_task`; normal tool permission and application staging grants still apply.
It returns the exact edit for separately authorized publication. Fresh model context does not mean
filesystem isolation. This first shorthand does not implement group delegation, coordinator waiting,
workspace selection, or automatic goal integration; connected tasks still require explicit edges.

`workflow.stage_agent_task` is plugin-owned shorthand for a single canonical
`AddNode` edit. Its typed request supplies run/revision/mutation identities, a complete
Agent `NodeDefinition`, explicit entry/exit flags, and reconciliation. The plugin
checks the node kind and prompt representation, then uses the existing authenticated
staging route and mutating-tool permission decision. The response returns the exact
canonical edit and `published: false`; publication remains a separate authorized
operation. Entry tasks consume run input. Non-entry tasks need connected graph edges;
this tool does not invent bindings, change authorization ceilings, or dispatch sessions.
It does not implement recursive child admission or durable parent waiting.

## Execution-scoped context

The workflow plugin exposes `workflow.execution_context` through the existing v1
workflow application invocation bridge. Its request contains only a page limit
(1–100), optional expected graph revision, and node/edge continuation cursors.
The host derives run, node, activation, and attempt identity from versioned session
provenance and verifies the exact stored execution link, active attempt, workspace,
and current daemon authority. Verification and graph reads share one non-mutating
store snapshot. Continuation without an expected revision fails closed; conflicts
require restarting pagination. Unknown request fields are rejected.

The response uses portable workflow contracts and contains authenticated execution
identity plus the existing bounded graph projection, a bounded canonical output
metadata page, and optionally one exact checksum-verified output. `after_output_id`
is an exclusive lexicographic cursor; continue until an empty page. This is a
snapshot query, not a durable stream: concurrently created outputs behind a cursor
require a fresh scan. `output_id` selects a value only within the authenticated run;
missing, oversized, or checksum-inconsistent values fail closed. Reads never open
artifact references or infer overall run completion from an individual output.
The plugin's `output_only: true` presentation option requires an exact `output_id`
and returns authenticated execution identity, revision and the complete verified
`output`, omitting unrelated graph/discovery/delegation data. Listed output
`inspection_arguments` select this view automatically. It cannot be combined with
delegation views, does not relax host checks or size limits, and is not a guarantee
that large values fit the tool-output budget. Missing/truncated values remain
uninspected evidence; the report must not claim otherwise. Contribution values
still require comparison with integrated artifacts and observed combined checks.
These fields extend the still-unreleased context operation, not a durable format.
This read-only operation does
not grant edit/publication authority or expose other runs. The optional objective
planning procedure allows this tool for inspecting its current execution. Output
inspection supplies existing results, not delegated task admission or durable waits.

## Purpose

Runtime workflow authoring lets a human, CLI, SDK, frontend, plugin, or generated producer describe a
workflow while Bcode is running. Every producer uses the same versioned, serializable application
contract and receives the same structured validation result. A renderer or generator may improve how
that contract is created, but neither owns workflow semantics.

This architecture extends the existing workflow domain. It does not introduce another workflow
engine, scheduler, canonical store, or frontend-specific definition format.

## Source-controlled JSON and TOML

Editable source files are serialization adapters for the same canonical
`WorkflowAuthoringDocument`; they do not define a second graph, compiler, store, or execution path.
`bcode_workflow::decode_workflow_authoring_source` is the portable SDK decoder, and the high-level
`bcode::workflow` facade re-exports it with `WorkflowSourceFormat` and
`WorkflowAuthoringDocument`. Decoding is deterministic, bounded by the authoring-document limit, and
runs normal semantic validation before returning.

Format selection is explicit or inferred only from a `.json`, `.workflow.json`, `.toml`, or
`.workflow.toml` file name. Callers never try one parser and silently fall back to another. JSON and
TOML sources that describe the same document must produce identical canonical source digest,
executable digest, requirements, effects, and compilation preview. The paired fixtures under
`fixtures/workflows/` mechanically enforce that property.

TOML cannot natively represent JSON `null`. Within JSON-valued schema, configuration, predicate,
transform, and presentation fields, an exact table containing only `$bcode_null = true` represents
one JSON null. Any table combining `$bcode_null` with another field, or assigning a value other than
`true`, fails closed. The marker is consumed by the format adapter and never enters canonical
workflow state. Normal omitted optional Rust fields remain ordinary TOML omission and do not require
the marker.

The plugin-contributed template/status/TUI convenience namespace is `workflow-ui`; it is deliberately
separate from the core `workflow author|start|cancel-computation` application namespace so enabling
the bundled workflow plugin cannot replace portable authored-workflow operations.

The CLI reads and bounds the local file, resolves its format, decodes it through the workflow-owned
adapter, and sends only the typed document through existing public application requests. Supported
operations are:

* `workflow author-check --source <path>` for daemon-authoritative validation and compilation preview;
* `workflow author-create --source <path> --draft <id>` for a new logical workflow and initial draft;
* `workflow author-update --source <path> --workflow <id> --draft <id> --generation <n>` for an optimistic generation-checked replacement.

An explicit `--source-format json|toml` overrides extension inference. The local path is not sent to
the daemon, persisted, or used as an authorization fact. Source changes never publish, activate,
start, or overwrite a draft implicitly. Existing author-publish, activation/start, inspection, and
portable export-bundle import remain separate application operations.

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
plugin-block defaults, permitted predicates/transforms, run limits, or initial input.
Bindings use the existing bounded transform language or a separately versioned bounded extension;
they never execute arbitrary code. The fully bound result is validated again as an exact definition.

Optional authoring presentation metadata is versioned, bounded, and namespaced. It may store graph
positions, grouping, comments, or editor hints. It is excluded from executable identity and cannot
affect topology, type checking, compilation, admission, authorization, dispatch, or persisted run
outcomes. A client can ignore an unknown presentation namespace and still understand the workflow.

## Producer-neutral authoring

UI, CLI, SDK, plugin, and generated producers discover the same bounded portable catalogs and submit
the same authoring document. Catalogs describe durable node kinds, block contracts, prompt profiles,
skills, predicates, transforms, schema dialects, production capabilities, and limit bounds without
leaking implementation objects. Model-backed prompt nodes explicitly choose a canonical structured
result or input-preserving side-effect completion; provider capability negotiation determines how a
structured result is produced and is not part of authoring semantics.

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

## Composable coding workflows

The broader product architecture for state-preserving operation dataflow, deterministic predicate
extensions, typed repeat outcomes, canonical terminal output, exact child-workflow calls, repository
verification authority, progress-document interactions, semantic graph editing, and prompt-generated
drafts is defined in [`composable-coding-workflows.md`](composable-coding-workflows.md).

Those extensions preserve this document's source/compile/run boundary. Coding-product state belongs
to the workflow plugin, operation behavior remains domain/plugin-owned, and only generic typed
contracts enter `bcode_workflow`. Child calls target exact immutable revisions or definitions; active
revision lookup is never deferred to dispatch. Editors and generators continue to submit the same
portable `WorkflowAuthoringDocument` and receive the same diagnostics.

## Mechanical enforcement

Domain-owned behavioral and compatibility tests should verify that:

* `bcode_workflow` does not depend on frontend, renderer, daemon, database, persistence, provider
  implementation, or plugin implementation packages;
* workflow-owned source does not import known terminal, web, daemon, database, provider-private, or
  plugin-runtime implementation types;
* only `bcode_workflow_store` owns the canonical workflow database path and authoring tables; and
* durable registration/start continues to use production capability admission.

Focused model, persistence, IPC, and integration tests will enforce version rejection, canonical
identity, presentation neutrality, optimistic conflicts, atomic publication, active-run pinning, and
producer-neutral behavior as those contracts are implemented.
