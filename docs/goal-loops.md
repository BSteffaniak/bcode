# Goal setup for prompt loops

`/goal` and `/loop` can open from a fresh sessionless screen. Opening or cancelling an
unsubmitted modal creates nothing. Valid submission creates a session in the current working
directory, applies draft model/provider/agent/reasoning selections, and attaches it before
generation or workflow dispatch. Configuration and attachment retries reuse the created
session. Existing-session invocations continue using that session. Control commands still
require a session. Closing after creation may leave an empty session but does not launch work.

`/goal` opens a goal setup modal owned by the bundled loop plugin. Enter a goal;
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
workflow run UUID is the document scope. New runs get distinct documents; pause/resume
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
