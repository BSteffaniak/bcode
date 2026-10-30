# Interactive Tools Architecture

Bcode interactive tools are split into three ownership layers:

* **Semantic interaction controllers** own state and behavior. They consume renderer-neutral
  `InteractionInput` values and expose typed snapshots.
* **Shared session view** projects exchange request and terminal resolution events into one stable
  renderer-neutral `Interaction` transcript item.
* **Renderers** adapt a client environment to those semantic inputs and snapshots. Terminal
  renderers translate key, mouse, and paste events; future browser renderers can translate DOM
  events.

This intentionally avoids a generic component protocol. Plugins model their own domain snapshots.

## Plugin author path

Most plugins should implement `bcode_plugin_sdk::interaction::PluginInteraction`:

```rust,ignore
impl PluginInteraction for MyInteraction {
    const KIND: &'static str = "my.plugin.interaction";

    type Request = MyRequest;
    type Snapshot = MySnapshot;

    fn new(request: Self::Request) -> Self { /* ... */ }
    fn snapshot(&self) -> Self::Snapshot { /* ... */ }
    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput { /* ... */ }
}
```

Registration is one line:

```rust,ignore
registry.register_interaction::<MyInteraction>();
```

Terminal rendering is optional and separate:

```rust,ignore
registry.register_interactive_surface::<MyInteraction, MyTerminalRenderer>();
```

`MyTerminalRenderer` implements `TerminalInteractionRenderer<MyInteraction>` and is responsible
for:

* reporting the complete logical row extent at a width
* rendering a bounded logical row slice using stable full-content coordinates
* optionally reporting the focused control's logical row range
* mapping terminal events to renderer-neutral `InteractionInput`

The host preserves explicit event disposition: ignored input may continue through root routing,
locally consumed input stops there, and semantic input is delivered once to the controller. Terminal
plugin surfaces may ask `PluginTuiHost` to resolve a key stroke into the host's configured
composer-like edit command, selection motion, or submit intent. This keeps configured editor
bindings consistent without exposing Bcode keymap enums to plugin controllers or shared session
contracts.

## Request metadata

Interactive tool requests carry a producer ID plus a versioned exchange schema and opaque payload.
A selected frontend adapter contributes an `interaction_kind`, the semantic controller kind used by
clients that support native interaction snapshots and inputs.

Native surface identity is not stored in the shared session-view contract. Each frontend resolves
its own adapter from the producer ID, exchange schema/version, and platform ID. A TUI adapter may
then provide a native `tui_surface_kind`; web clients may select a different controller or native
presentation from the same shared exchange envelope.

For example, the question adapter resolves:

* `interaction_kind = "bcode.question"`
* `tui_surface_kind = "bcode.question.inline"`

The durable exchange event remains canonical. `SessionView` first projects producer ID, exchange
schema/version, response policy, and opaque payload without interpreting plugin behavior. A client
adapter may then enrich the same interaction item with its semantic interaction kind while resolving
native presentation only at its frontend boundary. Resolution updates that same transcript identity
rather than creating renderer-local history.

Non-terminal clients should key off `interaction_kind` and should not need BMUX or terminal event
types.

## TUI presentation

The TUI owns interaction placement, clipping, hit testing, mouse-coordinate translation, cursor
adaptation, and terminal painting. The same plugin controller and surface can be presented:

* inline at the semantic interaction transcript item, using the indexed transcript as the only
  scrolling authority; or
* pinned above the composer as an overlay that does not alter transcript layout.

Inline surfaces reserve their complete bounded logical extent in the indexed transcript. The host
retains that extent as a sparse span and asks the plugin to draw only the visible slice; there is no
question-level viewport or full-height scratch allocation. Committed TUI geometry records placement,
logical extent, visible logical offset, and destination for drawing, cursor projection, hit testing,
and mouse translation. The host caps logical extent at 4,096 rows and each visible render at 512
rows and 131,072 cells.

Pinned surfaces use a TUI-owned viewport keyed by interaction identity. The host reveals the
renderer-reported focused row, clamps state across resize/replacement/shrink, and underpaints the
complete dock before bounded slice rendering. Neither mode owns canonical answers or exchange
lifecycle.

## Lifecycle and failure behavior

* Requests and terminal resolutions share one stable interaction transcript ID.
* Controller validation and answer construction remain plugin-owned.
* Failed response delivery retains controller and renderer-local text-edit state for retry; a
  surface closes only after authoritative success or confirmed prior resolution.
* Canonical terminal outcomes are absorbing. Duplicate/stale requests, hydration, and conflicting
  later terminal delivery cannot reopen or replace the first authoritative result.
* Remote resolution closes the active surface and updates the semantic transcript item.
* Unknown adapters remain generic, bounded transcript items rather than being guessed as a known
  interaction type.

## Session-owned durable interaction recovery

Architectural decision (2026-10-28): unanswered recoverable interactions belong to the
session, not the terminal window, browser mount, or client connection. The same backend
contract serves terminal and browser consumers. Disconnecting the last consumer is not
an answer, cancellation, or permission to discard an acknowledged selection.

Implementation is staged; this section describes the target contract, not a claim that
restart recovery is already available.

### Authority and producer contract

* Keep the existing controller transition state ephemeral. Add a separate, explicit,
  versioned producer checkpoint/restore/continuation capability; do not persist arbitrary
  runtime controller state implicitly.
* Producers validate and export bounded durable checkpoints. The host stores opaque
  producer data and never interprets question options, answers, or validation rules.
* Pin session, original invocation and exchange identities, producer/schema and
  checkpoint versions, and a monotonic revision. Restore requires compatible producer
  code and current execution authority; a checkpoint does not grant execution permission.
* Local focus, geometry, pointer capture and unacknowledged editor state remain
  attachment-local. Acknowledged semantic edits are session-owned durable state.

### Commit and recovery ordering

1. Commit the initial recoverable wait before advertising it as recoverable.
2. Validate each semantic mutation through the producer, compare its expected revision,
   and commit its checkpoint and mutation identity before acknowledging success.
   Exact retries return the same committed outcome; conflicting identity reuse fails.
3. Commit the first terminal resolution and its continuation identity before acknowledging
   submission. Terminal state is absorbing; stale edits cannot reopen it.
4. On rejoin, serve the authoritative checkpoint through the producer, not the original
   request replayed as a fabricated controller snapshot.
5. On daemon restart, recognize verified recoverable waits before generic abandoned-tool
   terminalization. A producer-owned continuation converts the committed resolution into
   the original tool result, using canonical invocation publication fences. Never rerun
   the original tool to reconstruct an unknown outcome.
6. Missing, incompatible, corrupt or incomplete checkpoints preserve evidence and expose
   an explicit unavailable/degraded interaction. They do not authorize raw-request
   fallback, silent cancellation, or tool re-execution.

Checkpoint and terminal-result storage must use the existing canonical session authority
and bounded transactional APIs. No renderer cache, parallel persistence store or
connection-local controller registry becomes a second authority. Schema changes must
follow session writer-epoch and migration admission rules.

### Original accepted-turn continuation (staged, disabled)

Producer `Complete` publishes the original tool result; it does not by itself resume the
model. This proposed integration remains disabled behind `PUBLICATION_ENABLED = false`.
The following schema/API names are an integration proposal, not implemented interfaces.
Storage/model ownership must be agreed before adding the server continuation module.
No historical reconstruction or runtime migration is part of this path.

#### Canonical envelope and ownership

The session domain owns `DurableTurnContinuationV1`, its codec, canonical events,
checkpointed locators, transactional transitions and receipts. The server owns capture
and restoration of deliberate loop semantics, configuration verification, and execution
ownership transfer. The coordinator owns model-loop/dispatch wiring and schema approval;
the interaction host binds the selected exchange and pure completion to this obligation.
Portable DTOs contain no runtime guards, credentials, raw tool arguments or renderer state.

The proposed envelope contains:

* Identity: schema version, session/submission/original turn/model-work IDs, accepted
  submission and activation event references, continuation ID, revision, and batch ID.
  Each event reference pins sequence, expected kind and a versioned canonical source
  digest. Pin the effective accepted execution-descriptor reference and a separately
  domain-separated digest of that complete descriptor, including its config recipe;
  `config_identity` alone does not prove descriptor equality. Define digest encoding in
  the session codec, not via independent host serialization.
* Loop state at `AwaitingBatchSettlement`: `provider_round`, `completed_tool_rounds`,
  `max_tool_rounds: Option<u32>`, next assistant segment order and output position.
  Capture after `begin_tool_round` has charged this batch and after successful-provider
  recovery reset, but before dispatch; do not charge it again on resume. Capture both
  proactive compaction boundary/attempt sequence options as deliberate loop state.
* Recovery state: context-pressure and malformed-argument recovery flags, max-token
  continuation count, consecutive retry count, per-rule retry counts, structured-output
  finalization flag, correction count and bounded correction-error text. Instruction is
  `None | MalformedToolArguments | MaxTokens | MaxTokensIncompleteToolCall`, never
  arbitrary prompt text. Resolve these variants to existing host instructions. These
  fields preserve loop semantics; this record is not a provider `RetryCheckpoint`.
* Batch: immutable ordered membership with invocation ID, tool source identity,
  canonical positioned-request reference and output position for each member; selected
  recoverable invocation/exchange identity; and captured scheduling-policy identity.
  Every member is source-bound before any dispatch, including not-yet-started siblings.
  Per-member state is `Pending | Started { dispatch_id } | Settled { result_ref,
  outcome }`; outcome uses canonical success/failure/cancellation semantics, not a
  guessed interpretation of tool text. Result references pin original invocation,
  canonical terminal event identity/digest and publication receipt. No copied output.
* Continuation phase and operation receipts: `AwaitingBatchSettlement`,
  `ReadyForModel`, `ModelDispatchStarted { dispatch_id }`, or `Retired { successor_ref }`.
  Readiness is derived only from verified settled membership, never a caller boolean.

All vectors, strings, references and encoded envelopes require session-owned bounds,
validated before allocation/publication and again on read. Reuse existing identity,
source, result, retention and retry limits; validate counters against the original
execution policy, preserving unlimited `max_tool_rounds = None`. Preserve `parallel`,
`max_concurrency` and preparation timeout without confusing concurrency with membership.
The inspected tool policy has no total batch-size cap: approving a shared admission and
storage membership/byte budget is a blocker, not permission to invent a 256-call limit.
Oversized batches must be rejected for recoverability before dispatch/readiness, never
truncated or advertised recoverable. Define bounded receipt retention with safe exhausted
budget behavior; eviction must not allow a previously committed operation to execute again.

#### Proposed transactional APIs and execution phases

1. `prepare_turn_continuation(capture, operation_id, guard)` commits the immutable
   membership and loop envelope before any batch dispatch or recoverable readiness.
   Canonical request references and checkpoint creation must share an atomic publication
   boundary, or dispatch must remain fenced until every source reference is committed.
   The existing session commit guard must additionally validate the accepted activation,
   effective execution descriptor/config context, and actual held execution authority.
2. `transition_turn_continuation(key, expected_revision, operation_id, transition, guard)`
   CASes each `Pending -> Started` before invocation and each `Started -> Settled`
   with canonical result publication in the same transaction. Pure producer `Complete`
   binds its original result to the selected member atomically; it never invokes that
   tool again. Exact operation-ID/payload retries return the original receipt;
   conflicting reuse and stale revisions reject without effects. Check sibling receipts
   instead of republishing their results. Policy denial/pre-dispatch cancellation needs
   an explicit canonical no-dispatch settlement transition, not a fabricated start.
3. `read_turn_continuation(key, expected_revision)` returns one bounded consistent
   canonical snapshot: envelope, exact accepted activation/descriptor sources, complete
   membership and referenced result receipts. Check locator freshness/source digests;
   no event-range/history scans, client binding, or raw request reconstruction. A crash
   with `Started` but no result is unknown execution and remains nonterminal/unavailable.
   `Pending` siblings also block model readiness; this recovery path dispatches neither.
   Committed results are reused, not tools. A compatible selected durable wait may
   complete through its producer contract; that is not general sibling replay authority.
4. `claim_turn_continuation(..., guard)` CASes to `ReadyForModel` only after every
   member's canonical terminal result is verified. Retire the interaction wait only
   with a durable replacement obligation, retaining a locator through every crash window.
   The server consumes the recovered admission binding into an original-turn permit,
   retaining the session lease without a release/reacquire gap. Non-workflow execution
   retains session/turn authority; workflow execution additionally retains a typed current
   run-authority guard with cancellation/dispatch association. Revalidate at commit and
   execution boundaries. Neither `workflow_authorized: bool` nor a stored owner ID is proof.
5. A distinct `ResumeOriginalTurn(proof)` entry validates the original config recipe,
   provider/model/profile/auth policy and current authorization, restores the envelope,
   and enters the post-batch loop boundary exactly once. Missing/request-only recipes,
   changed config or revoked authority defer without substituting daemon defaults.
   Do not synthesize a user request, activation, `ModelTurnStarted` or `ToolCallRequested`;
   do not call the batch dispatcher or use `recovering: true` provider-retry restoration.
   Apply the original loop's post-batch provider-round advancement once at the durable
   phase transition, not once per recovery attempt. CAS `ModelDispatchStarted` before
   the next provider request; a crash with no canonical provider outcome remains ambiguous
   and cannot automatically repeat it. Retire only with a durable successor/outcome
   reference. Local CAS does not promise exactly-once remote provider execution.

#### Integration blockers and acceptance gates

Schema/digest/budget approval, transactional batch hooks (including streamed/pipelined
request publication), pinned accepted-activation reads, consuming permit transfer, typed
workflow authority retention, and the dedicated model-loop entry remain required wiring.
The coordinator must confirm the complete semantic state at the post-batch boundary;
uncaptured control state blocks resume rather than silently resetting it. No standalone
DTO module or gate enablement establishes end-to-end delivery.

Before enabling publication, source-level behavioral/crash tests must exercise the real
host/storage/dispatch path: restart before/after every start/result/claim/handoff boundary;
selected pure completion plus committed, pending and unknown-execution siblings; duplicate
completion/CAS races; changed config and stale workflow authority; and resumed provider
counters/output positions. Assert original tool invocation counts do not increase, every
canonical result is reused once, incomplete batches remain nonterminal, no duplicate
turn-start/request events appear, and stale owners cannot dispatch or terminalize. Include
non-workflow and workflow runs and actual browser/native restart acceptance; DTO-only tests
are insufficient. Publication stays disabled until coordinated builds and these tests pass.

### Delivery plan and acceptance

1. Define opt-in producer wire contracts and canonical checkpoint/receipt records.
2. Implement transactional storage with revision checks, retry receipts and bounded
   discovery, including crash-window tests.
3. Implement the question producer's export, restore and post-resolution continuation.
4. Integrate session-owned host admission, detach/rejoin, recovery and terminal
   publication. Preserve existing behavior for tools without recovery capability.
5. Verify actual terminal and browser paths: acknowledged nonterminal edits survive
   detach/rejoin and abrupt restart; answering yields one canonical exchange resolution
   and one original tool result; duplicate delivery, stale revisions, concurrent clients,
   missing producers and incompatible checkpoints fail safely. Retain the real browser
   restart acceptance test rather than replacing it with synthetic state injection.

Implementation ownership is divided into producer contracts, canonical persistence,
shared host lifecycle/recovery, and renderer acceptance. The coordinator owns interface
integration, manifests, schema/version decisions and final coherent validation.

## Future server/client lifecycle

The daemon/server can host interaction controllers using plugin interaction registries:

* open controller by `interaction_kind`
* return `snapshot_json()` for a pending interaction
* accept `InteractionInput`
* return updated snapshots or submitted/cancelled results

This keeps browser support straightforward without introducing a browser renderer now.
