# Goal setup for prompt loops

## Live supervision

Use `/goal.watch` from the originating conversation to watch the associated run.
The read-only view refreshes a bounded workflow snapshot every two seconds and pins
that run for the lifetime of the view. It shows execution stages, attention, available
results and limitations. Press `1`–`9` to open a listed execution session using the
normal session viewer and return to supervision; Escape returns to the conversation
without stopping work. Observation failures retain the last preview, label it unavailable
and disable child navigation until a successful refresh. `/goal.status` remains the
portable text snapshot; `/workflow` provides advanced exact-target decisions and controls.
This is not a completion verifier, and absent child links or evidence are not success.


New goal evaluators use a fresh authenticated workflow context for canonical-output
inspection (not filesystem isolation); plain loop context behavior is unchanged.
Production-path deterministic tests discover revision-pinned output pages, inspect
exact contribution values, retain their IDs in delivery, and compare the report to
canonical outputs and the combined artifact. This is scripted coverage, not live-model
acceptance or host verification of positive evaluator claims. The isolated Git
conflict scenario also retains the observed integrated commit and checkout,
references both inspected canonical worker outputs, and lists all retained checkouts.
Its read-only evaluator inspects the resolved artifact and the revision recorded after
combined verification; this does not independently rerun validation or certify worker
commit provenance. New evaluator guidance distinguishes base, contribution and integrated
revisions, asks for working-tree state, and forbids creating commits just for reporting.

New goal evaluations may retain an optional `delivery` report in their canonical
output: `version: "1"`, integrated targets, inspected canonical contribution output
IDs, original criteria with passed/failed/unverified evidence, combined commands and
workspaces with observed outcomes, retained workspaces, and unresolved work. Lists
and text are schema-bounded. `/goal.status` previews targets, contribution references,
criterion statuses and evidence, combined check commands/workspaces/outcomes, unresolved
work and retained workspaces. Each category is limited to ten entries and each text
field to 320 characters; omissions are explicit.
`/workflow` retains the full result. Worker, approval, wait, failure and execution-session
previews also show at most ten entries each. Omission notices count only entries hidden
from the current snapshot, not the whole run; use `/workflow` for further inspection.
These are evaluator-reported observations, not
host-verified receipts or automatic Git integration. Missing reports remain unknown;
ordinary goals do not require collaboration. Unknown report versions reject.
Existing admitted graphs are not rewritten. The judgement block is version 4 for
the extended state schema; incompatible older bindings require normal compatibility
handling, not silent reinterpretation. Newly authored collaboration goals pin
`delivery_required=true` on normal and approval-resume safeguard entries. Evaluators
cannot bypass delivery safeguards by omitting or nulling the report (or returning
`delivery_required=false`). This requirement is authored graph policy, preserved by
source-preserving delegation; authorized graph revision remains possible. Historical
state without the field defaults to false; ordinary goals retain their existing policy.
New loops always run the loop-owned completion safeguard after agent evaluation,
including agent-only loops with no judgement provider configured. Reported blockers,
failed or unverified criteria/checks, empty target/criterion lists, blank target or supplied
contribution identities, blank criterion descriptions or check commands/workspaces, and unresolved work
withhold completion. The optional judgement evaluator additionally receives the delivery
report along with prose evidence and cannot override those negatives, even with fallback
enabled. Model confidence is an additional gate: it cannot promote an agent evaluator's
`condition_met=false` into completion, even at probability 1.0. Agent-only safeguarding
makes no provider request. It also withholds an affirmative decision with empty,
blank, or oversized prose evidence, using the judgement-backed evidence bounds.
The service boundary validates supplied delivery reports against the declared V1 schema
before interpreting them; deserialization alone does not enforce list or string bounds.
This is conservative rejection,
not independent verification of positive claims. Supplied contribution IDs are now
resolved through the versioned, invocation-scoped workflow evidence application service
before an affirmative decision. The host checksum-verifies each exact output in the
invoking run; callers cannot select a different run. Missing, corrupt, unsupported or
unavailable evidence withholds completion even with judgement fallback. The additive
`inspect_output_provenance` operation returns the bounded, checksum-verified canonical value
and its immutable activation-bound producer; older hosts reject the unsupported operation.
The loop consumer checks envelope compatibility, producer identity and revision, and currently
rejects adapted contribution dataflow rather than risk hiding blockers inside an envelope.
It rejects explicit blockers and
remaining work in standard `bcode.delegated_task_result.v2` contributions, including malformed
required fields. Custom contribution schemas remain authorable but unknown; neither an
unknown schema nor an empty blocker list proves success. Integrated-target identity,
criterion coverage and freshness of checks remain unverified by this safeguard.
Consequently new affirmative evaluations carrying a
V1 delivery report are withheld with an explicit target-bound-verification blocker, even
when references authenticate, checks claim success, or judgement fallback is configured.
Reports may additionally declare `content_scope`: one `{target, workspace, roots}`
entry for every `integrated_targets` label. The loop rejects duplicate/missing targets,
relative workspaces, overlapping or non-normalized roots, and scopes without an exact
workspace/root match in a check execution reference. This is an explicit coverage claim,
not proof that the chosen scope includes every relevant input, or a freshness certificate.
Historical reports omit it; positive delivery remains withheld in either case.
Check execution references may additionally declare `content_roots` (1–64 relative
paths). When supplied, these must exactly match the canonical admitted shell plan's
`observe_files`, including order; omitted roots preserve historical reports. This
binds a scope assertion to the executed observation, not to current filesystem state
or the entire delivered target. It does not enable positive completion. Shell advanced
command plans may also supply `expected_content`, an exact previously observed content
object, alongside `observe_files`. The shell owner re-observes those scopes after normal
execution authorization and rejects mismatches before running any command. With a
precondition, it also re-observes after each command: changed or unreadable content
stops the batch, retains executed command outcomes, and makes the plan unsuccessful,
even if the last command exited successfully or requested continuation. A final
observation checks the precondition again. This binds verification to selected integrated
content (including directory additions), not an arbitrary claim of success. Omitting observations while supplying a precondition fails
closed. The precondition is part of the prepared command plan identity; omitted
preconditions preserve existing behavior. It neither locks files against concurrent writers
nor certifies delivery-time freshness or complete target coverage. The loop evidence
consumer requires that exact precondition when recognizing a check: equal before/after
snapshots alone can conceal a batch that changes content, checks it, and restores it.
This does not detect changes restored within a single command and does not establish
complete input coverage or delivery-time freshness. Older shell plugins
reject the new field. Advanced plans may use empty `commands` with nonempty
`observe_files` for an observation-only execution. This retains the existing exact
preparation, approval, workspace confinement, and cancellation path; preparation
exposes absolute workspace-bound read-path policy facts rather than an empty shell
command. Traversing or absolute observation paths are rejected during preparation,
before policy or filesystem access. No process is
spawned, and successful output requires equal before/after observations. Empty
commands without observation scopes remain invalid. Older shell plugins reject
empty commands. This gives callers an authorized current observation at that
activation, not a delivery-time freshness certificate or a successful executed check.
A check reference may supply `observation_output_id` for a later observation-only
shell execution. The loop resolves it through the same invocation-scoped execution
evidence service and requires the same run, supported shell producer, successful
empty command plan, unchanged precondition, exact selected roots, workspace and
checked content. Missing or mismatched references reject the check; omission retains
historical report behavior. Timestamp ordering rejects older observations but does
not prove causal ordering or exclude intervening writes. This linkage does not
remove the positive-delivery safeguard or establish complete target coverage.
The report and original criteria are retained. This is a conservative safety restriction,
not a positive verification implementation: a canonical target/content-bound observation
path is still required before such delivery can be certified. The safeguard applies
even when the incoming agent verdict is negative: a configured judgement provider
must not promote an omitted required report or an unverified V1 report into
completion. Such reports return before provider dispatch. Historical reports remain
readable and terminal outcomes are not rewritten. Ordinary goals without delivery reports
retain their existing behavior and do not gain a collaboration requirement.
Existing admitted graphs are unchanged;
new graph-derived execution allowances account for the additional block.

Large nested corrective inputs can exceed the default tool-output context budget.
Use revision-pinned recipe parts and retained-output inspection, not guessed fields.
The scripted server acceptance test uses an explicit 16,000-character budget because
its fake provider cannot autonomously follow retained-output guidance; this is not
live-model usability evidence or a change to production defaults.

`/goal.status` includes a bounded execution snapshot from the application's semantic
workflow view: agent task names and states, execution-session identities, pending tool
permissions, mutation approval identities/operations/workspaces and reconciliation warnings,
input/approval prompts, failure diagnostics, and canonical terminal result
identity when available. For a resolved canonical loop result, it also shows the evaluator's
criteria verdict, summary and up to ten bounded evidence entries. A finished workflow with
`condition_met: false` remains explicitly unsatisfied. Missing or unsupported result detail
is surfaced, never replaced by a worker's result. Evidence is reported by the evaluator,
not independently verified by the status display. This is not a complete history or proof of integrated completion.
Refresh with `/goal.status`; use `/goal.worker <session-id>` to open a listed execution
session's canonical transcript and inspect its pending requests using normal session controls.
Opening a worker does not approve a request, resume work, or grant permission. The command
checks membership in the current bounded goal snapshot; use `/workflow` for older executions,
full details and exact approval controls.
Unavailable or unsupported detail is reported without hiding the existing goal controls.
An unavailable progress document is reported separately without hiding execution status
or allowance-continuation guidance.
The run summary and execution detail are separate observations and can advance between reads.
Status also explains the controls appropriate to the observed run state:
* Running: `/goal.pause` stops new attempt admission, not settlement of admitted work;
  `/goal.stop` requests cancellation without undoing effects. Refresh to observe the outcome.
* Paused: `/goal.resume` remains subject to ownership, compatibility and allowances;
  it does not approve pending requests. Admitted work may still finish while paused.
* Repair required: inspect `/workflow` and possible effects before using `/goal.detach`,
  which releases only the session association, not execution authority or unresolved work.
* Failed: continuation is offered only when the application recognizes a supported source.
  Completed and cancelled runs do not offer resume; inspect retained results and workspaces.
These hints are not authorization or a promise that a control will succeed against a newer state.

`/goal` and `/loop` can open from a fresh sessionless screen. Opening or cancelling an
unsubmitted modal creates nothing. Valid submission creates a session in the current working
directory, applies draft model/provider/agent/reasoning selections, and attaches it before
generation or workflow dispatch. Configuration and attachment retries reuse the created
session. Existing-session invocations continue using that session. Control commands still
require a session. Closing after creation may leave an empty session but does not launch work.

`/goal` opens a goal setup modal owned by the bundled loop plugin. Goals receive
plugin-owned optional coordination guidance during implementation, whether or not progress
notes are enabled: work directly when appropriate, or use authorized graph publication and a
durable continuation when delegation helps. This guidance does not grant extra execution
attempts or require workflow tools for single-agent work. Goal implementation uses a fresh
workflow execution context so execution-scoped tools can authenticate its activation; this
is not filesystem isolation or an authority grant. Plain loops retain their existing
implementation instructions and shared-parent context. Existing admitted definitions are
not rewritten. Live end-to-end optional delegation remains unverified.
Use `/goal --worker-attempts 20` to explicitly authorize extra node-execution attempts
while leaving collaboration optional. `/goal --collaborate --worker-attempts 20` instead
requests collaboration. The extra allowance does not change goal rounds, concurrency,
recursive policy, tool permissions or publication authorization; omission adds no attempts.
Enter a goal;
additional guidance and maximum iterations are optional. A blank maximum uses the
existing loop default (20). Generation captures the source session's bounded normal
model context at submission, including portable compaction summaries and projected tool
exchanges. It does not scan the repository or replay the full session history.

* **Ctrl+Enter** generates prompts and starts the loop after validation.
* **Ctrl+R** generates prompts for review. Edit the iteration prompt and stop
  condition, then use **Ctrl+Enter** to start.
* **Ctrl+P** toggles the default-on living progress document (`[x]` in the modal title).
* **Esc** closes setup. A late generation response cannot start a loop after closing.

Inputs are frozen while generation is pending. Failure preserves the draft for retry.
Generation uses the normal bounded, tool-free structured-generation application API.
The plugin bundles generation, iteration, and evaluation guidance in `prompts/`.
The original objective and additional guidance are included as task data in both
resulting prompts; executing agents inspect repository instructions and referenced
specifications through normal tools. Generated text cannot change permissions or
iteration limits. Normal model usage costs apply to generation.

Once started, this is an ordinary loop: the existing implementation turn and
read-only completion evaluator repeat under the existing durable workflow runtime.
There is no separate goal store, lifecycle, or scheduler. `/loop.status`,
`/loop.pause`, `/loop.resume`, `/loop.stop`, and `/loop.detach` work normally.
Equivalent `/goal.*` aliases control the same session loop, including loops started
with `/loop`. Detach is only the existing repair-required operation; it does not
resolve an ambiguous prior operation or mean background execution.

An existing active loop must be explicitly replaced through the usual confirmation;
it is not cancelled merely to generate prompts. Exhausting the iteration allowance
is not evidence that the goal was achieved. Generated stop conditions and evaluator
claims remain fallible and should be checked against current evidence.

## Continuing after the iteration allowance is exhausted

Use `/goal.continue --worker-attempts <positive integer>` to explicitly extend the
execution-attempt cap of the currently associated active goal. The command inspects
bounded canonical allowance facts and submits the exact run and expected/target caps
through the existing authorized control. Transport retries reuse that exact request;
conflicts are reported rather than rebased. It does not add iterations, unpause work,
or increase a separate composition-root cap. `/goal.status` points exhausted active
goals to this intervention; `/goal.resume` remains necessary for paused goals. Missing
or unknown allowance facts fail closed. This wiring has deterministic contract coverage;
a goal-command IPC exhaustion/resumption experiment remains required.

Use `/goal.continue <additional_iterations>` (or `/loop.continue`) to authorize more work:

```text
/goal.continue 10
```

This grants **up to ten additional implementation/evaluation iterations**, stopping early when
the existing condition is met. It preserves accepted prompts, session context, evidence, and the
progress document; it does not regenerate instructions or rerun goal initialization. `/goal.resume`
remains the control for a paused run.

Continuation requires the exact associated run to have a verified, settled repeat-limit failure.
Success, cancellation, unrelated failure, ambiguous operations, foreign/unverifiable ownership,
and pending replacement are not restart shortcuts. Damaged or unsupported historical checkpoints
fail closed. The initial implementation supports bounded leaf-run graphs (at most 1,000 nodes and
1,000 edges); composed execution requires a separate continuation policy.

Each grant creates a linked successor with new activation identities. The predecessor remains
terminal with its original history. Runtime counters record per-run completion and cumulative prior
iterations; status shows lineage and the new grant. The original run UUID remains the explicitly
inherited progress-document scope across successors. Missing documents are reported, not recreated.

Admission atomically checks source ownership, graph/output identity, and the current association,
then records the grant, successor, and lineage. Exact request retries return the same successor;
conflicting or competing grants are rejected. A lost command response is retried once with the same
request identity. Inspect status after a transport failure rather than assuming no grant committed.
Dispatch recovery uses the ordinary workflow runtime. Existing absolute deadlines, concurrency,
retry policy, authorization ceilings, and session-wide limits are not reset by an iteration grant.

Workflow-store schema 40 adds continuation lineage and an indexed exhaustion lookup. Existing
stores upgrade through the normal exclusive-owner, backup-preserving initialization path.

## Conversation-aware generation

The daemon captures a generation-pinned source model-context view, checks that the source
has not changed during capture, and creates a separate generation session in the source
working directory. Source events stay in bounded daemon memory and are projected using the
normal model-message rules when constructing the tool-free structured request. They are
never appended as copied messages to the destination session. Only normal generated output,
execution capability identity and accepted provenance are durable. Other structured generators
without a source retain their previous behavior.

Captures expire after ten minutes, are released at turn completion, and are limited to 32
pending captures of at most 4 MiB each. Lost/expired capabilities fail closed, including after
daemon replacement; regenerate rather than resuming without context. Opaque provider-managed
source compaction is rejected instead of copying provider-private state. Oversized requests
use the normal context estimator/capacity check and require explicit source compaction rather
than silently dropping history or persisting an automatic summary of request-only content.

The generator resolves conversational references, retains applicable decisions and later
corrections, and excludes unrelated prior tasks. Ambiguous objectives return a clarification
question without launching work. Host-verified source session/generation/cutoff provenance is
separate from model output and is retained in accepted prompts and progress-document setup.
A fresh generation captures fresh context; workflow-start retries reuse accepted prompts.

## Shared activity and readable output

The existing persistent status chrome consumes renderer-neutral activity presentation from
`bcode_session_view::presentation`, correlated with the existing plugin run-status contribution.
It does not maintain another goal tracker. The latest correlated activity supplies stage and
iteration context; run status remains authoritative. Without a current contribution, historical
transcript activity cannot recreate a persistent active goal. Narrow-terminal layout remains BMUX-owned.

Setup, terminal assistant output and web assistant output share a bounded structured-output
formatter. Partial prompt strings are readable implementation/stop-condition fields; they remain
provisional and never change execution inputs. Malformed or unsupported object output shows a
receiving/unavailable notice instead of a raw JSON fallback. Internal producer envelopes are not
shown in normal web activity details. Canonical model output is retained unchanged.

## Live goal activity

Prompt generation observes the generation session through shared semantic snapshots, including
provider-exposed reasoning and assistant output. Draft output is not accepted instructions.
The setup view shows elapsed time and time since its last semantic update. Scroll with the
normal text-view keys or mouse wheel; Tab folds the output. `h` opens the ordinary live session
viewer and returns to the retained setup on exit. Esc requests cancellation and waits for the
operation outcome; it does not silently start a goal from a late result. Hiding output or opening
the session viewer does not cancel the operation. Providers may expose no intermediate text.

Initialization publishes a plugin-owned activity distinct from implementation iterations.
Model-turn completion retains active workflow/runtime-work status instead of flashing idle.
`/goal.progress` opens a read-only, scrollable saved-document view, refreshing bounded reads
at most once every two seconds while open. It pins the selected run; closing and reopening
selects the current association. Saved notes and checklist counts are not readiness or completion.
Errors retain the previous preview with an unavailable notice; they never reconstruct notes.

Native plugins must be rebuilt against plugin ABI 5, which adds the observable generation
host method and explicit observation/cancellation handle. ABI 4 libraries are rejected.

## Living progress document

By default, starting a goal prepares a Markdown document under the owning session
store's `session-artifacts/<session-id>/working-documents/<workflow-run-id>/progress.md`.
The application resolves the path; the modal never guesses a state root. The existing
workflow run UUID is the document scope. Independent new runs get distinct documents; explicit
continuation successors inherit their original run's document scope. Pause/resume
and start retries retain the same document. Read-only `/goal.progress` displays a bounded
snapshot and `/goal.status` includes its path. Missing documents are reported, not repaired.

Preparation happens after generation/review and replacement confirmation, before workflow
submission. It is explicit, create-if-absent, bounded to 64 KiB, and atomically publishes the
initial file without overwriting existing edits. Failed starts may leave useful prepared
notes. Closing during preparation prevents launch but may leave the authorized draft.
The confined preparation implementation currently supports Unix; unsupported platforms fail
closed, and users can disable the option explicitly. No special filesystem permission grant
is added: agent updates use ordinary authorized tools.

The initial scaffold is explicitly unresearched. Bundled `goal-progress-document.md` and
`goal-progress-template.md` adapt the local-progress-doc skill and its shared completion
contract without depending on a user skill, Nix configuration, or interactive skill invocation.
With progress notes enabled, the lifecycle is prompt generation → repository research and
document initialization → LLM readiness assessment → implementation. A separate durable
agent activation receives the source conversation and loop input, researches the repository,
and updates only the progress document. It does not implement the product. Guidance asks for
goal-specific phases with **checkboxes**, dependencies, exit criteria, evidence and validation,
but the LLM owns their organization and readiness; there is no document-shape validator or
extra approval on the ready path. Product closure and architectural integrity remain distinct.

A concrete unresolved blocker waits through the existing workflow input mechanism. Resolving
that input reruns research against the same document before implementation; input alone is
not readiness. Ordinary cancellation, permissions and durable activation recovery apply.
Interrupted initialization retains useful edits rather than replacing them with a scaffold.
Initialization does not consume an implementation iteration. Plain loops and goals with
progress notes disabled retain their existing execution flow. Existing runs retain their
persisted definitions and instructions; upgrading does not silently rewrite an active goal.
Reconcile an old document through an explicitly authorized planning-only turn before resuming.

The plan remains mutable throughout execution: add/split/reorder/refine/remove planned work,
record significant decisions, preserve completed evidence and reopen disproven checkboxes.
Read its current phases and next actions each iteration, not just the opening status lines;
check current state, and update affected sections in place. Do not narrow the original objective.
Keep the current plan prominent, compact and below 64 KiB. Summarize historical evidence rather
than accumulating prior-status or previous-increment narration.
The evaluator reads but does not edit; checked boxes and a Done heading are not proof.

These files are mutable working notes, not canonical event history or finalized artifact
references. They are not registered as finalized compression candidates. Their association
is the versioned session document contract plus the exact run UUID, not parsed prompt text.
The resolved path and bundled instructions are also persisted in the ordinary loop input,
so resumed execution retains them without a separate goal registry. No new workflow state
machine or goal scheduler is introduced.

Closing setup does not undo a workflow start already submitted to the host. Use
loop status and controls to inspect or stop admitted work. Live objective editing,
new resource budgets, automatic workflow authoring, and a durable goal object are
outside this feature.
