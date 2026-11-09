# Managed harness integration

The disableable `bcode_harness_plugin` (`bcode.harness`) exports `static_plugin()` and
five advanced tools/palette commands: `harness.start`, `inspect`, `followup`, `respond`, `interrupt`.
Each accepts `request_json`, a bounded `ManagedSessionRequest` containing the shared
`HarnessRequest`, optional `session_id` (instance inspection), optional creation-only
`parent_session_id`, and optional explicit `assistance_destination_session_id`.
Parentage and destination hints never grant response/approval/control authority; only the
active authenticated invocation of a child's canonical parent has delegated control.
Mutation correlation must equal the retained open/submission/response/control ID.
No auth secrets or native login inputs are accepted by these model-facing operations.

## Model-facing delegation shapes

When the plugin configuration has at least one enabled instance, three more tools are offered
(`harness.delegate`, `harness.continue`, `harness.result`), and `harness.respond` /
`harness.interrupt` additionally accept a simple form. These map onto the same managed-session
application operations; workflow blocks and commands keep the `request_json` form only.

* `harness.delegate { instance_id?, prompt }` resolves the instance from this plugin's catalog
  (the configured default when omitted), opens an ordinary child of the calling session in its
  workspace and submits `prompt` as the first turn, then returns `{session_id, instance_id,
  status, submission_id}` without waiting for the native turn. Session titles are not part of
  the open contract.
* `harness.continue { session_id, prompt }` reads the child's retained target through the
  `binding` operation and submits the next turn.
* `harness.result { session_id, after? }` reads bounded canonical history (`history`
  operation, which first drains one bounded native page) and returns activity
  (`running`, `settled`, `needs_attention`, `interrupted`, `failed`, `idle`, `unknown`), the
  latest result text preview, pending interactions (request ID, kind, prompt, tool and input
  previews, choices) and a cursor. Pending detection is advisory; responses are host-validated.
* `harness.respond { session_id, request_id, approve | answer [, choice_id] }` and
  `harness.interrupt { session_id }` read the retained target and send a typed response or
  interruption.

Identities are derived (SHA-256, domain-separated) from the calling session, the host tool
invocation ID and the normalized input: child session (UUID v8), open, submission, attempt
(UUID v8), response and interrupt mutation IDs. Re-running the same invocation reuses the same
retained identities, so the host returns recorded evidence or reports uncertainty instead of
repeating a native mutation. A delegate retry first reads the derived child's binding: a
confirmed target means the first turn was delivered and is reported as `already_submitted`;
a prepared target resubmits the byte-identical retained submission. A different invocation never
re-sends an uncertain mutation; uncertain outcomes are reported with the child `session_id` and a
hint to poll `harness.result`. Preparation binds the normalized input, host workspace scope,
resolved instance and route in the descriptor; invocation rejects any change.

Delegated control (see `docs/managed-external-sessions.md`) is what allows the parent's
invocation to address the child: the host admits only the active invocation of the child's
immutable canonical parent. The default build agent enables these tools and receives short
guidance only when `harness.delegate` is offered; the plan agent cannot delegate, continue,
respond or interrupt.

Submission retains `HarnessSessionTarget::Prepared` when native identity is not yet known.
The first user submission uses this durable prepared target without inventing a native session.
`BindingConfirmed` may promote observation/control routing only after the shared confirmation
validator agrees with the original open, instance, version, selection and generation. The
retained first submission stays byte-equivalent and is never rewritten/replayed. Confirmation
alone is not execution completion.

## Application callback

The plugin requests `bcode.managed-session/v1`, not the native `bcode.harness/v1` service.
Operations map to `Open`, `Inspect | Observe | Reconcile`, `Submit`, `Respond`, `Interrupt`.
The callback payload is the managed request object, not the outer request_json wrapper.
The response is `ToolInvocationServiceResolution::Responded { payload }` containing a
serialized `HarnessServiceResponse`; the plugin checks the exact operation/correlation,
family, binding and generation using `validate_for`, bounds and reserializes it before
presentation. Public failures never contain raw native diagnostics. No failure retries
an ambiguous mutation.

Tool preparation requires exactly one host-issued invocation service route and binds
operation, exact payload and route. The host advertises it in service-routes context v1
and independently checks authenticated invocation/plugin/target authority on callback.
Commands have no tool route ID; their correlation is the retained harness request ID,
not authentication. Workflow callbacks use the dispatch identity and no tool route ID.
Each callback scope must be authenticated by its host bridge closure.

## Workflow blocks and execution owner

All six manifest blocks require plugin-owned preparation. The five operation blocks
return operation evidence only. `harness.execute` accepts the same request_json shape as
followup: a retained `Submit` operation against an already admitted/open ordinary managed
child of the workflow parent, in the same workspace. Its output contract is
`bcode.harness.assignment-result/v1` with session/submission/native-turn identities and
bounded result text. Mutation permission and exact workflow grants precede dispatch.

Preparation validates the typed request, emits ordinary tool-policy metadata and a
versioned exact-input checksum/descriptor. The host operation owner validates the pinned
block/input/preparation against its manifest contract. It persists an ordinary execution
session link and stable `bcode.server.managed-harness/v1` recovery receipt before native
delivery. The handle retains exact dispatch identity, application request, input/dataflow
and schemas, but is not native acceptance or completion. Duplicate dispatch returns it
without redelivery. A crash between publication and delivery requires reconciliation,
not implicit replay. Delivery errors leave observation-owned uncertainty.

`packages/server/src/harness_workflow.rs` supplies:

* `dispatch_block(&Arc<ServerState>, &PreparedActivationDispatch) -> Result<Option<Value>, WorkflowStoreError>`.
* `observe_receipt(&ServerState, &AttemptReconciliationRequest) -> Result<AttemptObservation, WorkflowStoreError>`.
* `cancel_receipt(&ServerState, &ActiveAttemptCancellation) -> Result<(), WorkflowStoreError>`.
* `authorize_call(state, run_id, dispatch_identity, session_id, plugin_id, &ManagedSessionCall)
  -> Result<AuthorityGuard, WorkflowStoreError>` for the managed application admission path.

The admission verifier reads exact canonical receipts/links and grants only the retained
submission, exact binding observation/reconciliation, and deterministic interruption
under durable workflow cancellation intent. The returned workflow authority hold must
survive runtime delivery. Managed runtime separately fences canonical session ownership.
It must not be replaced with fake plugin invocation registration or parent-derived control.

Observation uses bounded indexed submission/native-turn history, drains one bounded native
page through the same managed application, and recovers its native acknowledgement cursor
from the actor's exact atomically committed `ExternalHarnessState::observation_after`.
Oversized evidence windows are explicit
unavailability/deferred state, never end-of-history or success. Idle, result text, daemon
loss and interrupt acknowledgement cannot settle work. Only complete explicit settlement
for the exact native turn plus correlated result passes ordinary block and node schemas,
normal dataflow adaptation and workflow `ValidatedOutput`. This preserves mixed graph
state-envelope composition without a second scheduler or session event store.

Cancellation retains the exact dispatch-derived control identity. Its acknowledgement does
not settle the workflow or prove absence of effects. Native assignment cancellation cannot
be inferred until the shared harness contract provides authoritative correlated cancellation
outcome evidence; current code conservatively continues observing.

## Central connections

`workflow_operations::WorkflowOperationReceiptObserver` and `signal_operation_cancellation`
route the new owner without model-turn decoding. The representation owner cancellation and
failed-start observation paths use these wrappers. Central lib wiring must route ordinary
plugin-block dispatch to `dispatch_block` before synchronous invocation, periodic receipt
observation to `observe_receipt`, and cancellation to `cancel_receipt`, plus declare the module.

The managed dispatcher/admission needs an authenticated Workflow caller carrying run ID,
exact dispatch identity, execution session and plugin owner. It calls `authorize_call` and
holds that guard through the runtime call; no general cross-session invocation authorization
is granted. All workflow managed operations use the installed `managed_sessions` application
backend rather than vendor service dispatch.

The integration wave has passed canonical host harness tests and isolated offline CLI
acceptance, including restart, cancellation, retained parentage and terminal handoff.
Production workflow-boundary tests prove correlated completion with a controlled
native fixture. These are not live mixed-harness acceptance: general native
background settlement, interruption and resumed-turn support remain subject to
adapter evidence and must fail closed when unsupported.
