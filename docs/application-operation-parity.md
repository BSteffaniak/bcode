# Application operation parity

Mutation approval CLI commands print the resolution and exit 1 when the requested decision was not applied (including expiration). Explicit denial recorded as denied is a successful decision, not a successful workflow execution. Decision matching is workflow-owned and independent of continuation.

Mutation resolution includes optional request-local `continuation`: `not_required`, `driven`, or `failed`. Absence means unavailable (older/store-only result). A continuation failure does not erase the committed approval resolution. CLI prints the result and exits 1 on `failed`; inspect before retrying. `driven` does not mean the run completed, and continuation metadata is not persisted.

Mutation-approval continuation follows the resolved outcome, not the requested action. Only approved resolutions with a nonempty grant identity and pending activation request continuation. Expired approvals and already running/terminal observations do not drive workflow work again; normal workflow ownership fencing still applies.

Mutation-approval resolve decision/result contracts are workflow-owned (`WorkflowMutationApprovalDecision`, `WorkflowMutationApprovalResolution`). Store compatibility exports preserve type identity and wire shape; routing, client and CLI name domain contracts. Resolution orchestration and persistence remain unchanged.

Approval/denial history exposes `resolution` using the same mutation-approval decision disclosure as aggregate decisions. The payload outcome must match the event kind; otherwise resolution is unavailable. Denial retains its normalized `reason`. Scope remains private and history never confers current approval authority.

`mutation_approval_requested` history reuses `WorkflowMutationApprovalInspection` after validating scope version and run/node/activation binding. Private operation facts and digests remain excluded. Invalid scope/binding reports `invalid_mutation_approval`; history is informational and does not establish that an approval is still pending.

Activation creation/waiting history exposes `WorkflowActivationObservation`, excluding input. `wait_kind` distinguishes input from approval waits and is null for pending activations. Run identity and the deterministic node/generation-derived activation identity must agree; pending/waiting status must match the event kind. Invalid records report `invalid_activation` without repair.

`attempt_prepared` history exposes `WorkflowAttemptPreparationObservation` (positive attempt number, known side-effect classification, preparation time), never dispatch intent. Invalid facts produce `invalid_attempt_preparation`; preparation is not admission or authorization.

History events with a verified indexed dispatch identity now include `attempt_correlation` encoded as `WorkflowHistoryAttemptCorrelation` (run/node/activation/attempt). Coordinates come from the validated attempt record, never producer payload text. Missing or foreign records remove the digest and report `correlation_unavailable`; corrupt records fail the read without repair. This is bounded diagnostic correlation, not execution authority or durable resume.

Pending mutation approvals (aggregate inspection and both list operations) use `WorkflowMutationApprovalInspection`. Its informational scope preserves operation identity, workspace, bounded input summary, resource claims, capability and reconciliation policy, but excludes owner-prepared operation facts and preparation/input digests. This is an intentional wire-contract change within the exact-artifact client/daemon boundary; it cannot deserialize as an executable private approval. Resolve by approval ID; enforcement loads the unchanged private scope. Input summaries remain user content, not secret-filtered diagnostics. The bounded wait/activation input surfaces also remain explicit workflow content; this change does not claim they are sanitized.

Aggregate decisions use `WorkflowDecisionInspection`. Recognized mutation-approval decisions expose `{"availability":"mutation_approval","approval_id":...,"approved":...}` after validating the decision identity and private scope's run/node binding; scope content is excluded. Other decision values remain `{"availability":"withheld"}`. Observations are not executable decisions; stored values remain unchanged.

Aggregate grants use `WorkflowGrantInspection`, retaining identity/timing/usage. Valid mutation scopes matching the grant's run and node expose `{"availability":"mutation","scope":...}` with the same review fields as pending approvals, excluding private facts and digests. Invalid, unsupported, or mismatched scopes remain `{"availability":"withheld"}`. Recognized policy grants expose `{"availability":"policy","scope":...,"capability":...}` only after domain identity validation and grant/run/node correlation. Unknown policy fields are withheld rather than guessed; omitted run scope retains its explicitly broader meaning. These observations are not executable authorization scopes; private store records and enforcement are unchanged.

Definition inspection retains exact stored JSON bytes alongside an immutable typed definition, so wire reserialization does not invalidate the stored-representation checksum. Typed callers use `definition()` or consume it with `into_definition()`; this does not independently verify checksum authenticity.

This inventory records how user-visible Bcode actions cross the application boundary. It is coverage and architecture documentation, not a runtime registry or source of product semantics.

The canonical implementation path is:

See [`application-operation-boundary.md`](application-operation-boundary.md) for the implemented ownership split between focused operations and transport adapters.

```text
CLI, TUI, HyperChad, or SDK caller
→ typed client or shared renderer action
→ local IPC adapter
→ exhaustive server request routing
→ owning session, runtime, workflow, permission, plugin, or configuration domain
→ typed result or bounded ordered event stream
```

## CLI error exit status

`CliError::exit_code` maps returned failures to process status: 1 for runtime/I/O failures, 2 for invalid arguments, JSON, or malformed/unsupported exchange resolutions, 3 for execution-policy turn rejection and recognized authorization denial, and 4 for recognized cancellation. Turn rejection because the session is unavailable is a runtime failure (1). Server-code classification uses exact codes, not substrings: `invalid_exchange_resolution` maps to 2; `authorization_denied` and `workflow_operation_unauthorized` map to 3; `cancelled`, `workflow_computation_cancelled`, and `worktree_create_cancelled` map to 4. Unknown server codes remain runtime failures (1). These statuses do not imply rollback or safe retries of committed operations.

Provider authentication's typed `Cancelled` terminal outcome also maps to status 4; generic login failures remain status 1 regardless of their message text. Credential handling is unchanged.

This describes returned-error classification, not complete command parity: signal handling, successful responses containing failed terminal outcomes, and end-to-end authorization/cancellation verification remain separate work.

## Local plugin CLI behavior and evidence

`plugin list` and local `plugin services` inspect selected manifests without loading native libraries. `plugin check` loads selected native plugins and executes activation/deactivation callbacks; it is not a read-only manifest validator. Local `invoke`, `call`, and `publish` use a local plugin host. With `--daemon`, services/invoke/call/publish instead target the running host; combining `--daemon` with local `--root` is rejected rather than silently ignoring the roots.

Explicitly disabled plugins are excluded even when also listed as enabled. Current process tests verify that a disabled plugin with a missing library does not break list/services/check/publish, and that direct invocation by ID or interface fails without a success receipt. A selected plugin with a missing library remains discoverable, but check/invoke/call/publish fail with a native-library-load diagnostic and empty stdout.

CLI-owned plugin output uses fallible writes; output failures return runtime exit status 1 rather than panicking. A plugin service response containing an error also returns runtime exit status 1 in both local and daemon modes, after writing its human or JSON response envelope; successful responses return 0. Plugin-owned error codes are not interpreted as host authorization or cancellation codes. This does not roll back plugin actions: publication, invocation, or lifecycle callbacks may already have completed before a receipt write fails. Callers must not infer retry safety from an output failure. Plugin-generated output is outside these CLI-writer guarantees.

Evidence lives in `packages/bcode/tests/cli_process.rs` and the plugin-output writer tests in `packages/cli/src/lib.rs`. Run `bash scripts/test-cli-plugin-process.sh` to build the existing metrics plugin and run the process suite including opt-in native-plugin tests. The runner obtains the library path from Cargo artifact output, including custom target directories. Native tests cover successful and failed local invoke/call responses in human and JSON modes; they do not establish daemon service behavior, permission equivalence, or semantic-controller completion. Manifest-only and absent-library fixtures separately establish discovery, selection, output, and loading-failure behavior.

## Classifications

* **Shared application** — daemon-backed product behavior that needs a typed API and, where useful non-interactively, a CLI path.
* **Frontend user state** — a persisted preference owned by one frontend. It is not canonical session state and does not mutate declarative configuration.
* **Frontend local** — presentation, focus, draft editing, navigation, or other behavior that does not cross the application boundary.
* **Offline/lifecycle** — explicit maintenance, local artifact validation, credential custody, plugin development, or daemon lifecycle behavior whose owner is intentionally outside routine daemon operations.

## Shared operation inventory

`Client/API` names the existing typed path where one is known. `Gap` is deliberately explicit: an underlying request or client method does not count as complete CLI parity.

| Stable ID | Source actions | Classification and owner | Client/API and server path | CLI and machine output | Authorization, cancellation, and current gap |
| --- | --- | --- | --- | --- | --- |
| `session.create` | new session, first message without a session | Shared application; session | `BcodeClient::create_session_in_working_directory`; session lifecycle routing | `session create [NAME] [--json]` | Path confinement and applicable policy precede creation. Spawn-safe idempotency remains a gap. |
| `session.catalog` | session picker, `/sessions` | Shared application; session catalog | session list/catalog client methods; session lifecycle routing | `session list --json` | Reads remain bounded, best-effort, and non-mutating. |
| `session.open` | picker selection, HyperChad session navigation | Shared application; session/session view | attach/open and projection APIs | `attach`, `tui`; no canonical one-result JSON contract | Attachment state is explicit and artifact-isolated. |
| `session.rename` | rename effect | Shared application; session | `SessionViewAction::RenameSession`, `BcodeClient` rename | `session rename SESSION_ID NAME [--json]` | Canonical event history remains authoritative. |
| `session.delete` | delete effect | Shared application; session | `SessionViewAction::DeleteSession`, `BcodeClient` delete | `session delete SESSION_ID --yes [--json]` | Explicit non-interactive confirmation precedes deletion; authorization remains canonical. |
| `session.working_directory` | `/cwd`, worktree attach | Shared application; session/worktree | `SessionViewAction::ChangeWorkingDirectory`, typed client method | `session set-working-directory SESSION_ID PATH [--json]` | Paths are canonicalized and confined before mutation. |
| `session.history` | older/newer transcript, timeline, context inspection | Shared application; session/session view | bounded history, around, inspection, and projection APIs | `session history`, `around`, `inspect`, `timeline`, `export` | Routine paths are bounded and non-mutating; export is explicit. JSON coverage varies. |
| `session.search` | `/search`, session picker search | Shared application; session search | typed search/status/explain APIs | `session search` family | Normal search is bounded; purge/rebuild/backfill are explicit maintenance. |
| `session.import` | rescan/import effects | Shared application with plugin-owned source behavior | plugin import service and canonical import API | `session import` family | Import paths are confined; imported history becomes canonical only through the import owner. |
| `session.submit_turn` | composer submit, `send` | Shared application; session/runtime | `SessionViewAction::SubmitMessage`; `BcodeClient::send_user_message_with_execution`; `submit_turn` for durable admission | `send SESSION_ID [MESSAGE\|--file FILE\|--stdin] [--idempotency-key KEY] [--background] [--json]`; `--follow-up` selects explicit queued placement | Input is bounded UTF-8 and exactly one source. Default automation returns exact canonical `TurnAdmission`; follow-up returns queue/disposition facts; immutable exceptional execution options remain explicit. |
| `session.invoke_skill` | `/skill`, skill picker | Shared application; skill/runtime | `SessionViewAction::InvokeSkill`; typed client invocation | `session invoke-skill SESSION_ID SKILL_ID [ARGUMENTS] [--json]` | Skill context remains daemon-owned; finite output reports canonical message acceptance. |
| `session.cancel_turn` | stop/cancel action | Shared application; runtime | `SessionViewAction::CancelTurn`; typed client cancellation | `cancel SESSION_ID [--clear-queue] [--json]` | Reports only cancellation request acceptance; authoritative terminal outcome remains observable through watch/history. |
| `runtime.inspect` | `/runtime`, runtime panel | Shared application; runtime work | typed list/history/watch methods | `runtime-work list/history/watch`, all with `--json` | List/history are bounded structured results; watch is JSON Lines with canonical events. |
| `runtime.cancel_work` | runtime cancel action | Shared application; runtime work | `SessionViewAction::CancelRuntimeWork`; typed client cancellation | `runtime-work cancel SESSION_ID WORK_ID [--json]` | Reports request acceptance while durable execution authority and terminal outcome remain runtime-owned. |
| `session.compact` | `/compact` | Shared application; session/runtime | `SessionViewAction::CompactContext`; typed client method | `session compact SESSION_ID [--json]` | Explicit operation; normal reads never compact implicitly. |
| `session.select_model` | model picker and `/model` | Shared application; model catalog/session | `SessionViewAction::SetModel`; typed client method | `session set-model SESSION_ID MODEL_ID [--provider ID] [--json]` | Resolution uses the centralized model catalog. |
| `session.select_reasoning` | thinking/reasoning dialog | Shared application; session/model semantics | `SessionViewAction::SetReasoning`; typed client method | `session set-reasoning SESSION_ID [--effort VALUE] [--summary VALUE] [--json]` | Uses provider-neutral reasoning semantics. |
| `permission.status` | effective agent-policy inspection | Shared application; agent-profile policy provider | `BcodeClient::agent_policy_status`; `session_operations::agent_policy_status` | `permission status [--json]` | Preserves provider source, fallback indicator, enabled-tool summaries, and degradation diagnostics. This is not authorization for a proposed operation; canonical policy still evaluates operation facts before effects. |
| `session.discover_agents` | agent picker | Shared application; agent-profile | `BcodeClient::list_agents`; server agent listing operation | `session agents [--json]` | Daemon-resolved profiles, not a CLI registry; no session ID required. |
| `session.discover_skills` | skills picker and skill inspection | Shared application; skill | `BcodeClient::list_skills`, `describe_skill`; server skill operations | `session skills [--json]`, `session describe-skill SKILL_ID [--json]` | List JSON preserves discovery diagnostics; manifest JSON preserves the typed skill contract. Human discovery diagnostics go to stderr. |
| `session.select_agent` | `/agent`, plan/build aliases | Shared application; agent profile/session | `SessionViewAction::SetAgent`; typed client method | `session set-agent SESSION_ID AGENT_ID [--json]` | Agent policy and permissions remain authoritative. |
| `session.auth_pool` | auth-pool picker | Shared application; auth/session | typed client preference method | `session set-auth-pool POOL (--profile PROFILE\|--clear) [--json]` | Secret custody remains provider/plugin-owned. |
| `session.skills` | skills palette, activate/deactivate | Shared application; skill/session | `ActivateSkill`, `DeactivateSkill`, typed client methods | `session active-skills`, `activate-skill`, and `deactivate-skill`, each with `--json` | Skill identity is typed and plugin/domain-owned. |
| `permission.inspect` | permission list/surface | Shared application; permission | typed list method | `permission list [--session-id SESSION_ID] [--json]` | Canonical normalized operation facts are exposed without secrets; filtering is over the bounded pending set. |
| `permission.resolve` | permission surface | Shared application; permission | `ResolvePermission`, `ResolvePermissionBatch`; typed client methods | `permission approve ID [--remember] [--json]`, `permission deny ID [--json]`, `permission resolve-batch BATCH_ID (--approve\|--deny) [--json]` | Authorization decision precedes the blocked side effect; batch resolution is canonical and duplicate terminal resolution is safe. |
| `interaction.inspect` | interactive tool surface | Shared application; plugin/tool exchange | `BcodeClient::list_pending_tool_exchanges`; `interaction_operations::list_pending_tool_exchanges` | `interaction list [--session-id ID] [--json]`; `interaction inspect EXCHANGE_ID` emits finite JSON | Request includes producer, schema, version, response policy, and bounded opaque payload. Inspection selects from the pending list; it neither reserves the exchange nor opens a semantic controller. |
| `interaction.invocation_input` | active plugin invocation input | Shared application; invocation/tool owner | `BcodeClient::send_invocation_input`; canonical invocation-input routing | `interaction input SESSION_ID --payload FILE [--json]` | Accepts a complete producer-schema `ToolInvocationInput` envelope; `-` reads stdin. CLI input is bounded to 256 KiB and the daemon enforces a 64-KiB encoded envelope limit. The receipt acknowledges enqueueing, not execution, controller state, or retry safety. |
| `interaction.drive` | rich TUI/HyperChad interaction | Shared application with plugin-owned semantics | HyperChad uses a plugin-owned `InteractionInput` controller locally; planned daemon-hosted controller contract still needs JSON snapshots and `InteractionOutput` | CLI gap | Preferred generic fallback. HyperChad validation/activate/change/submit behavior is integration-tested over real IPC, but dynamic daemon-hosted controller discovery and CLI access remain implementation work; unknown schemas are surfaced rather than guessed. |
| `interaction.resolve_raw` | generic exchange fallback | Shared application; plugin/tool exchange | typed `ToolExchangeResolution` client path | `interaction respond EXCHANGE_ID --payload FILE [--json]` and `interaction cancel EXCHANGE_ID [--json]` | Schema-aware fallback. The CLI reads at most 256 KiB of input JSON; the daemon separately limits the complete encoded resolution to 64 KiB, including envelope overhead and escaping. `-` reads stdin. Unknown versions remain producer-owned, and authorization/compatible-adapter checks precede resumption. |
| `worktree.list_create_remove` | `/worktree`, create/attach effects | Shared application; worktree | typed start/status/wait/create/remove methods | `worktree list`, `create`, and confirmed `remove`, with `--json` | Paths are confined; branch modes conflict explicitly; creation is addressable/idempotent; optional canonical session attachment/creation is domain-owned. |
| `workflow.author` | workflow control center/plugin surfaces | Shared application; workflow | typed authoring, validation, publication, and inspection methods | `workflow author` | Workflow contracts and persistence stay domain-owned; JSON is already common but must be inventoried per subcommand. |
| `workflow.execute` | workflow/Ralph actions | Shared application; workflow/runtime | typed start, inspect, waits, retry, input, approval, output, cancellation methods | `workflow start/runs/cancel-run/pause-run/resume-run/inspect-run/run-output/waits/retry-node/provide-input/resolve-approval/cancel-computation` | Durable workflow authority, permission, idempotency, and cancellation rules apply. `definitions [--limit N]` lists checksum-verified registered definitions; `describe-definition --definition-id ID --version N` returns one exact definition or JSON `null`. `runs [--limit N]` returns bounded run summaries. `run-status --run-id ID` returns one durable summary or JSON `null` when absent. `doctor --run-id ID [--limit N]` returns the bounded domain doctor report as JSON without applying repairs. `cancel-run`, `pause-run`, and `resume-run` require `--run-id` and print the domain's boolean result; cancellation acknowledgment is not proof that execution has terminated. `waits --run-id ID [--limit N]` returns a bounded JSON array of pending input/approval activations, not a resumable cursor page. `retry-node` requires `--run-id`, `--node-id`, `--activation-id`, and positive `--failed-attempt`; eligibility remains server-owned. A returned retry error does not prove rollback or authorize blind retry. |
| `plugin.service` | plugin commands and surfaces | Shared application when targeting the running host; plugin | typed discover/call/invoke/publish methods | `plugin ... --daemon` | Schemas are versioned and plugin-owned. Local mode is explicitly development/diagnostic. |
| `model.catalog_diagnostics` | model catalog diagnostics | Shared application; model catalog | `BcodeClient::model_catalog_diagnostics`; normalized server operation | `model diagnostics [--json]` | Bounded normalized diagnostics; provider-private data and secrets remain excluded. |
| `agent.spawn` | delegated Bcode agent | Shared convenience operation composed from session/worktree/selection/turn owners | planned result: canonical session plus `TurnAdmission` | CLI gap; final naming follows CLI inventory | Must authorize before effects and define retry safety across session creation and turn admission. No subprocess or child-run store. |
| `session.watch` | live TUI/HyperChad observation | Shared application; session view/frontend contracts | normalized snapshot/live event APIs | `session watch --json` and `runtime-work watch --json` emit JSON Lines | Initial state is bounded; envelopes distinguish snapshot, durable, live, runtime-work, and resync-required records. Resync ends the stream for explicit reconnect/replacement; no durable resume is claimed. |

## Source reconciliation checkpoint

Source inspection at `ebf6f738` distinguishes implemented access from remaining parity work:

* **TUI and HyperChad:** the guarded `SessionViewAction`, `SessionViewActionOutcome`, `BuiltinCommandId`, and `PresentationAction` inventories below classify their declared actions. HyperChad's `LocalInteractionControllers` in `packages/hyperchad/src/lib.rs` owns ephemeral local controllers and cleanup, not a daemon controller service. CLI invocation input and raw exchange inspection do not close `interaction.drive`.
* **Artifacts:** `session artifact-range` calls `BcodeClient::session_artifact_range` through `read_artifact_range_to` in `packages/cli/src/lib.rs`. The result is the session-owned `SessionArtifactRange`; JSON is the default and `--raw` writes bytes. TUI's `artifact_stream.rs` and HyperChad use the same client read. Artifact access is implemented, not a missing CLI operation.
* **Workflow graph:** `BcodeClient::inspect_workflow_run_graph` and `RuntimeAndModelRequest::InspectWorkflowRunGraph` exist. The workflow plugin's `workflow.graph` command invokes the bounded page API and returns `graph` in command options. The CLI now exposes `workflow inspect-run-graph --run-id ID --expected-revision N [--after-node-id ID] [--after-edge-id N] [--limit N]` and emits the typed page as finite JSON. Zero revision/limit is rejected before dispatch; stale revisions and graph validity remain daemon-owned. The request and all graph-page result types now belong to `bcode_workflow`; IPC compatibility re-exports preserve existing imports and serialization. Client, CLI, and reusable server graph operations name those workflow-owned contracts directly. This is separate from the older `inspect-run` result and does not provide a durable resume cursor.
* **Session import:** `handle_session_import_command` routes source discovery through the daemon plugin service and `open` through `BcodeClient::import_external_session`. Sources/discovery support JSON. `open` currently prints the session ID and warnings to stdout without a JSON option; machine-output parity remains missing. Routine import is not a local persistence bypass.
* **Session derivation:** typed client methods exist for snapshot, prompts, start, status, and cancellation. `SessionCommand` has no corresponding derivation subcommands. The top-level classification below describes domain ownership, not evidence of CLI derivation coverage.
* **Plugin operations:** explicit-ID invocation, interface calls, discovery, and publication have typed client paths and daemon CLI adaptation. Local native-plugin process tests prove local behavior only. Neither these paths nor the statically registered HyperChad controllers establish versioned dynamic daemon controller discovery/open/input/close.
* **Public adapter boundary:** focused server operation modules remain private and accept server-owned context. Typed client access exists; direct-operation tests do not establish an independently usable public in-process adapter contract.

This checkpoint distinguishes the concrete surfaces above. The nested inventories and client-call-site classification below extend reconciliation across workflow author/package commands and public async client methods. The enum guard checks classifications, not behavioral reachability, output contracts, authorization equivalence, or product closure.

## Nested workflow and plugin command inventory

These classifications follow the exhaustive CLI handlers in `packages/cli/src/lib.rs`. Finite workflow results use `print_json`; parsing or a listed command does not prove successful execution, authorization equivalence, or end-to-end closure.

### `WorkflowCommand`

* Shared authoring/package adapters: `Author`, `Package`.
* Shared execution and bounded observation: `Start`, `Events`, `Attempts`, `Definitions`, `DescribeDefinition`, `Doctor`, `RunStatus`, `Runs`, `CancelRun`, `PauseRun`, `ResumeRun`, `RetryNode`, `Waits`, `LaunchDetail`, `CatalogView`, `RunView`, `InspectRun`, `InspectRunGraph`, `RunOutput`, `ProvideInput`, `ResolveApproval`, `ResolveMutationApproval`, `MutationApprovals`, `CancelComputation`, `ReconcileOrphans`. These invoke typed client methods; doctor and orphan reconciliation retain their distinct inspection/mutation semantics.
* Offline/lifecycle: `MigrateStore`, `ResetStore` use explicit store maintenance; reset invokes `reset_incompatible_workflow_store_offline`, not the similarly named client method.

### `WorkflowAuthorCommand`

* Shared reads: `List`, `Get`, `Inspect`, `Draft`, `Revision`, `Catalog`.
* Shared mutations: `Edit`, `Create`, `Apply`, `Update`, `Publish`, `PublishAndStart`, `Activate`, `Archive`, `Discard`, `Fork`, `Preset`.
* Shared transfer/computation: `Export`, `ImportPreview`, `Import`, `ImportDraft`, `ImportRevision`, `Validate`, `Preview`. File decoding is CLI adaptation; validation/preview use the source-format APIs, not the older authoring-document client convenience methods. Computation controls carry operation identity and timeout; cancellation remains explicit.

### `WorkflowDraftCommand`

`List`, `Get`: typed bounded draft listing and exact draft reads; JSON results.

### `WorkflowRevisionCommand`

`List`, `Inspect`, `Get`: typed revision listing, inspection, and exact reads; JSON results.

### `WorkflowPresetCommand`

`List`, `Get`, `Create`, `Update`, `Delete`: typed preset operations; mutations preserve expected-generation contracts and JSON results.

### `WorkflowPackageCommand`

* `Discover` uses `workflow_launch_catalog` on the daemon and emits a bounded JSON page. It currently supplies no cursor/search/source-kind/readiness filters; those client capabilities are not all exposed by this command.
* `Validate`, `Preview`, `Apply` read a local manifest closure, then invoke daemon package validation/preview/application with computation controls. Local file preparation does not make the product operation local.
* `Publish` validates the supplied lock request and invokes daemon publication with expected generations. All five commands emit finite JSON.

### `PluginCommand`

* `Contributions`: daemon-only `BcodeClient::plugin_contributions` discovery, emitted as one JSON object containing `commands`, `command_contributions`, and `config_extensions`. No local-root option or command/effect execution is supported. The envelope is now owned by `bcode_plugin_sdk`, using command contracts and serde-only `bcode_plugin_models` manifest metadata. IPC and plugin-host re-exports preserve source and serialization compatibility; neither native loading nor host implementation types are needed to name the discovery metadata. The SDK envelope remains outside the models leaf because it composes the existing command contract dependency.
* `List`, `Check`: explicit local manifest discovery and native lifecycle checking, respectively.
* `Services`, `Invoke`, `Call`, `Publish`: local development mode or explicit `--daemon` routing to typed running-host operations. Generic payload semantics and command effects remain plugin-owned. `invoke_plugin_service`/`call_plugin_service` finish at `print_service_response` and `write_service_response` in the CLI: they write payload bytes (or the JSON byte envelope) and service errors, without decoding or executing `InvokeCommandResponse.effects`. Server `plugin_operations` likewise projects service results rather than executing frontend command effects. Workflow plugin commands perform their typed daemon calls before returning; many then place inspection results in `OpenPluginSurface.options` (`plugins/workflow-plugin/src/lib.rs`, `invoke_command`). Thus a successful mutation can occur through generic service invocation, but opening/operating its subsequent surface is not an automation fallback. Graph results are retrievable inside the producer-owned response, not a normalized CLI graph contract. No automatic effect execution is implied or added.

### Client methods without direct CLI call sites

A source scan of all public async methods in `packages/client/src/lib.rs`, compared with CLI call sites, identifies the following categories. Wrapper absence is not automatically a semantic gap:

* **Transport/renderer plumbing:** watcher `initial_snapshot`/`next_snapshot`, catalog/projection/workflow watchers, `ping`, metrics ingestion, runtime-context updates, subscriptions/catch-up, attach/open preparation variants, `recv_event`. Existing session/runtime watch and attach commands cover portions; catalog/workflow live watches are not thereby CLI-complete.
* **Equivalent/convenience paths:** `list_sessions` versus status-bearing lists; catalog refresh versus working-directory refresh; worktree start/status/wait versus `create_worktree`; `send_user_message`, `cancel_session_turn`, `resolve_permission`, `runtime_work_history`, and `invoke_skill_with_execution` versus selected option-bearing or paged paths. Do not invent duplicate commands solely for wrappers.
* **Explicit historical/maintenance distinctions:** full `session_history` is not required for normal reads; CLI export uses bounded pages. Older search index/backfill starts are not equivalent to the exposed complete-backfill path. Compatibility inventory and offline workflow reset retain maintenance ownership rather than routine frontend semantics.
* **Frontend state:** composer draft get/set and presentation-note append have canonical typed APIs but no direct CLI calls. Native edit mechanics remain frontend-local; shared persisted draft access must be assessed separately from gestures.
* **Daemon discovery:** `auth_pool_list` now has `auth pool daemon-status`, returning the provider-auth-owned secret-free summaries as finite JSON. Local auth pool list/status remain unchanged and are not substitutes for running-host state. `plugin_contributions` now has daemon-only `plugin contributions` JSON access; plugin service listings remain a different response.
* **Workflow gaps:** `apply_workflow_draft_edits` now has `workflow author edit --request FILE` (`-` for stdin). It decodes a bounded complete request, validates batch version/generation/bounds, and preserves server authorization and atomic expected-generation checks. Updated/conflict/rejected outcomes are printed as JSON; conflict/rejected outcomes exit 1 without fabricating a transport error or retry guarantee. The draft-edit request/result, draft snapshot, and optimistic conflict are workflow-owned contracts; IPC retains compatibility re-exports. Client, CLI, server execution, and TUI consume the domain types directly. Serialization and edit-version semantics are unchanged. Source-apply, create, replacement, and publication requests/results, immutable revision snapshots, and computation controls are also workflow-owned; IPC re-exports preserve existing wire defaults and names. Cancellation/deadline enforcement and authorization remain server-owned. Publish-and-start and run-start outcomes now use workflow-owned run summaries/provenance/status and a normalized admission failure, rather than store-owned records or IPC errors. Store and IPC compatibility exports retain wire shapes; a failed admission does not roll back publication. Lifecycle activation/archive/discard/fork, preset mutations and snapshots, and content-minimized authored inspection contracts are workflow-owned as well. IPC remains a compatibility transport adapter; the diagnostic guard checks domain-owned summaries. Import/export requests, collision-policy outcomes, authored run selection, and package-export start contracts are also workflow-owned with unchanged wire defaults and generation fields. Authoring keyset pages and source/package computation/apply/publication envelopes are workflow-owned too; query implementations retain bounded store access and validation retains explicit caller computation context. Runtime/template start and instantiation requests now consume workflow-owned allowance data; store/IPC re-exports preserve defaults and existing serialization. Orphan reconciliation reports, coordinator status, binding lookup, and associated-run control actions are workflow-owned. The broader run inspection envelope still exposes store-owned records and is not considered a portable application contract yet. Output summaries and bounded validated/terminal output inspections are workflow-owned, with compatibility exports preserving their version constants and wire shape. Run inspection now exposes a typed workflow definition snapshot instead of `StoredWorkflowDefinition`; the established `definition_json` wire field is decoded by a compatibility serializer, not by the TUI. Other inspection record dependencies remain open. Activation/input, wait-kind/wait, and attempt/side-effect summaries are workflow-owned with store compatibility exports; storage queries and dispatch decisions remain implementation-owned. Decision, grant, mutation-approval, and resource-lease observation data are workflow-owned as well, without moving authorization or mutation behavior. Event history now uses a workflow-owned diagnostic observation with explicit store-row conversion in standalone history and aggregate inspection. Authority-transfer events now project only typed numeric generation/time facts, withholding fencing tokens and private ownership evidence; malformed transfers return a fixed unavailable diagnostic. Failure-history kinds, fan-out failure and attempt pause now expose closed `WorkflowHistoryDiagnostic` reason codes rather than arbitrary owner text or identities. Known pause reasons remain actionable; unsupported reasons report unavailable. Run-view failure projection uses the same sanitizing conversion, not raw store payloads. Run created/resumed/paused/completed/cancelled events expose typed historical status derived from the recorded transition kind, not arbitrary payload content; this does not override current run state. Output-validation history exposes schema version, creation time and artifact presence, excluding output values and artifact locations. Malformed metadata reports unavailable; canonical output access remains separate. Wait-resolution history exposes typed input/approval kind and acceptance, rejecting unknown kinds and excluding submitted content. Attempt admission history exposes a typed positive attempt number and admission time, never owner receipts; malformed admissions report unavailable. Grant recorded/consumed history now exposes typed creation/expiration time, optional use limit and consumption count without scope content. Invalid bounded-use counters report unavailable; absent limits preserve unlimited semantics. Fan-out materialization and repeat settlement expose workflow-owned count/bound/outcome observations; decoding failures return explicit unavailable diagnostics and extra producer fields are discarded. Unreviewed event payloads are now withheld with `unavailable: unreviewed_event_payload`, rather than forwarded opaquely. Event kind, sequence, run identity and time remain available; canonical history is unchanged. Failure/pause payloads retain a dispatch identity only when it has the store's 64-hex digest representation. This restores bounded correlation without restoring free-text owner messages. Withholding is an explicit diagnostic fidelity limitation, not completion of the remaining semantic projections. The aggregate inspection envelope is workflow-owned with an IPC compatibility export. Raw payload semantic normalization remains open. Child-run links, descendant summaries, and repeat outcomes are workflow-owned; their queries and durable ownership checks remain store/server responsibilities. `workflow_launch_detail` now has `workflow launch-detail --request FILE` (`-` for stdin), consuming the bounded workflow-owned versioned request and returning JSON. Local contract validation rejects unsupported versions before dispatch; source resolution remains daemon-owned. `workflow_package_publication` also has no direct CLI call. Source validate/preview covers the CLI authoring path, not every older authoring API representation. `workflow.graph`, template/definition/start commands, and `workflow.repair` in the workflow plugin provide plugin-owned paths; direct CLI template access and attempt repair are not established by that fact. Graph paging now has a dedicated typed CLI path described above. Associated-run lookup/inspection/control are used by the loop plugin and TUI host, not dedicated CLI commands.
* **Ralph and derivation:** Ralph client operations remain plugin-domain integration, including lifecycle recording, rather than independent host workflows. Derivation snapshot/prompts/start/status/cancel remain typed-client capabilities without `SessionCommand` counterparts. Neither family is complete CLI parity merely because generic plugin invocation exists.

The inventory now classifies the inspected nested commands and absent direct client call sites. It deliberately retains missing semantic access and unproven generic-effect consumption as gaps; the next phase-1 item must establish per-operation contracts, authorization/cancellation behavior, and actual CLI execution rather than equating textual coverage with completion.

### Graph CLI product-path validation

`workflow_graph_cli_pages_real_daemon_and_rejects_stale_revision` in `packages/bcode/tests/cli_process.rs` launches an isolated foreground daemon, loads the actual default-agents plugin, creates a session, authors/publishes/starts the source-defined input fixture, and reads graph JSON through the CLI. It checks independent node/edge cursor continuation (including edge exhaustion) and stale-revision exit 1 with empty stdout. The fixture adds a second input node and an edge before publication. The owned child is killed/reaped before the temporary root is dropped. It does not exercise daemon reconnect recovery.

The native-plugin prerequisite is explicit; the test is ignored in the default suite. On macOS with the default target directory:

```sh
cargo build -p bcode_default_agents_plugin --quiet
BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY="$PWD/target/debug/libbcode_default_agents_plugin.dylib" cargo test -p bcode --features app --test cli_process workflow_graph_cli_pages -- --ignored
```

Set the environment variable to the actual Cargo artifact for other target directories/platforms. The fixture copies it to the library filename declared by the bundled manifest. Missing plugin setup fails rather than bypassing authorization.

## Frontend user-state and local inventory

| Stable ID | Source | Classification | Required handling |
| --- | --- | --- | --- |
| `tui.theme.selection` | theme picker, `/theme` | Frontend user state | Persist only through TUI state APIs in `tui.toml`; never mutate `bcode.toml`; no application/CLI parity requirement. |
| `tui.streaming.presentation` | streaming configurator, `/streaming` | Frontend user state | Persist only the TUI presentation override; no canonical session or cross-frontend meaning. |
| `frontend.draft.edit` | composer editing and `UpdateDraft` | Frontend local unless using the existing explicit draft persistence path | Renderer owns edit mechanics; explicit draft persistence remains a narrowly scoped session-view action. |
| `frontend.session.switch` | `SwitchSession`, picker/navigation | Frontend local navigation plus shared attach/open | The renderer chooses a session; canonical hydration uses shared attach/projection APIs. |
| `frontend.history.navigation` | `LoadOlderHistory`, `LoadNewerHistory`, HyperChad history window | Frontend local navigation over a shared bounded read | Renderer chooses anchors/direction; session view owns semantic projection. |
| `frontend.layout_input` | scrolling, cursor, focus, mouse, overlays, disclosure | Frontend local | TUI/renderer-owned; no CLI or application operation. |
| `frontend.palette` | slash, command, model, skill, session pickers | Frontend local discovery UI | Selection invokes the corresponding shared operation; palette layout/filter/focus remains local. |
| `frontend.presentation_note` | TUI presentation note | Shared persisted presentation only where already explicitly modeled | Must remain presentation-only and cannot affect execution semantics. |

## Offline, maintenance, and lifecycle inventory

| Stable ID | CLI families | Owner and reason for local execution | Safety boundary |
| --- | --- | --- | --- |
| `daemon.lifecycle` | `server start/run/status/probe/stop/cleanup/stop-all/retire-incompatible` | Daemon lifecycle coordinator | Exact artifact isolation and graceful/forced-stop policy remain authoritative. |
| `session.maintenance` | doctor, repair, reindex, migration, retired catalogs, owner release/stop/kill | Session repair, migration, and lease owners | Explicit command, confirmation where destructive, path confinement, compatible writer/lease checks, no invocation from normal reads. |
| `search.maintenance` | purge, rebuild, backfill | Daemon-backed session-search owner | Explicit derived-state operation; canonical history is never replaced by the index. |
| `workflow.store_maintenance` | migrate/reset store | Workflow-store owner | Explicit schema/confirmation path; optional-domain failure stays isolated. |
| `theme.artifact` | `theme list/validate/copy` | Local source validation/copying | Operates on requested theme artifacts and does not change interactive selection or declarative config. |
| `model.ignore_state` | ignore/unignore/ignored | Local configuration state owner | Explicit user state; model resolution still uses the central catalog. |
| `auth.custody` | auth/login/logout/security/profile/pool credential operations | Config, provider auth, plugin auth, and vault owners | Secrets never enter public session/frontend contracts or diagnostics. |
| `plugin.development` | plugin list/check and non-daemon invoke/call/publish | Explicit local plugin discovery/diagnostics | Must not be represented as acting on the running daemon host. Paths are confined and output is untrusted. |
| `workflow.package_local` | local manifest/source closure preparation | Source-controlled workflow package owner | CLI reads explicit local files; package discovery uses the daemon launch catalog, and validation/preview/application/publication use typed daemon operations. |

## Automation examples

### Local theme artifacts

`theme list --json`, `theme validate PATH --json`, and `theme copy BUILTIN PATH --json` emit finite JSON objects with `schema_version: 1`. List returns `themes` with `id`, `display_name`, `source`, `dark`, and `light`; successful validation returns `valid`, `id`, and `fingerprint`; successful copy returns `builtin` and `path`. Default human output remains available. Validation input is limited to 1 MiB. Copy requires `--force` to replace an existing destination; output failure does not roll back a completed copy. JSON copy rejects paths that cannot be serialized before filesystem mutation. These are local artifact operations, not interactive theme selection.

These examples use only supported typed CLI/application paths. Machine-readable one-result commands emit JSON; live watch commands emit one JSON object per line.

### Create and configure a session

```sh
session_id="$(bcode session create 'automation task')"

bcode session set-working-directory "$session_id" ./workspace --json
bcode session set-agent "$session_id" build --json
bcode session set-model "$session_id" model-id --provider provider-plugin-id --json
bcode session set-reasoning "$session_id" --effort high --summary detailed --json
bcode session activate-skill "$session_id" skill-id --json
```

The current create command deliberately emits the canonical session ID as one plain line. Subsequent operation results use the structured machine contracts shown below.

### Submit a durable turn

Submit text directly and retain the exact canonical admission receipt:

```sh
bcode send "$session_id" 'Implement the requested change' \
  --producer my.automation \
  --idempotency-key task-42-attempt-1 \
  --json
```

Read a bounded UTF-8 prompt from a file or stdin:

```sh
bcode send "$session_id" --file prompt.md --json
printf '%s' 'Inspect current status' | bcode send "$session_id" --stdin --json
```

Queue an explicit follow-up instead of requesting ordinary durable admission:

```sh
bcode send "$session_id" 'Then run focused tests' --follow-up --json
```

### Watch ordered session activity

```sh
bcode session watch "$session_id" --limit 100 --json
```

The first JSON Lines record is a bounded snapshot. Later records are durable or live events. A `resync_required` record ends the stream; reconnect to obtain a replacement bounded snapshot. This is state transfer, not a durable resume protocol.

Runtime-work activity has the same JSON Lines convention:

```sh
bcode runtime-work watch "$session_id" --json
```

### Resolve permissions and plugin interactions

```sh
bcode permission list --session-id "$session_id" --json
bcode permission approve PERMISSION_ID --json
bcode permission resolve-batch BATCH_ID --approve --json

bcode interaction list --session-id "$session_id" --json
bcode interaction respond EXCHANGE_ID --payload response.json --json
bcode interaction cancel EXCHANGE_ID --json
```

The interaction response payload follows the producer-owned schema and version returned by `interaction list`. Unknown schemas are not guessed. HyperChad already drives a plugin-owned semantic controller with `InteractionInput`; the daemon-hosted controller contract and CLI access remain tracked implementation gaps.

### Inspect and cancel runtime work

```sh
bcode runtime-work list "$session_id" --json
bcode runtime-work history "$session_id" --limit 50 --json
bcode runtime-work cancel "$session_id" WORK_ID --json
bcode cancel "$session_id" --clear-queue --json
```

Cancellation output reports request acceptance. Observe watch/history for the authoritative terminal outcome.

### Worktree control

```sh
bcode worktree list --cwd ./workspace --json
bcode worktree create task-name \
  --cwd ./workspace \
  --new-branch feature/task-name \
  --attach-session-id "$session_id" \
  --json
bcode worktree remove ../task-name --yes --json
```

Removal is intentionally confirmation-gated. Paths remain subject to daemon-owned canonicalization and confinement.

### Human-readable use

Search-backfill operation status output with `--json` uses compact JSON followed by a newline. Follow mode emits one flushed JSON Lines record per changed revision; one-shot status remains a single valid JSON value. Output write failures are returned as I/O errors rather than panics.

Omit `--json` for concise human summaries. Machine callers should use JSON/JSON Lines rather than parsing those summaries.

## Renderer coverage

HyperChad currently declares these shared application actions through `PresentationAction`: submit message, cancel turn, update draft, resolve one permission, resolve a permission batch, move a bounded history window, and resolve an interaction. Session navigation uses `PresentationContext::session_target`. The narrower mechanism inventory remains in [HyperChad Application Interaction Inventory](hyperchad-interaction-inventory.md).

The TUI uses `SessionViewAction` for renderer-neutral session mutations where appropriate and direct typed `BcodeClient` methods for other shared domains. `TuiEffect` also contains frontend-local orchestration and user-state effects; membership in that enum does not itself make an effect a shared application action.

## Mechanically checked source inventories

The architecture guard requires every current variant below to remain named in this document. That detects additions requiring classification without turning this document into runtime authority.

### `SessionViewAction`

`SubmitMessage`, `InvokeSkill`, `CancelTurn`, `ResolvePermission`, `ResolvePermissionBatch`, `ResolveExchange`, `SwitchSession`, `UpdateDraft`, `SetModel`, `SetReasoning`, `RenameSession`, `DeleteSession`, `ChangeWorkingDirectory`, `CancelRuntimeWork`, `CompactContext`, `SetAgent`, `ActivateSkill`, `DeactivateSkill`, `LoadOlderHistory`, `LoadNewerHistory`.

### `SessionViewActionOutcome`

`None`, `MessageAccepted`, `Cancelled`, `PermissionResolved`, `PermissionBatchResolved`, `InteractionResolved`, `SessionRenamed`, `SessionDeleted`, `WorkingDirectoryChanged`, `RuntimeWorkCancellationRequested`, `ContextCompacted`.

### `BuiltinCommandId`

`Version`, `Sessions`, `Search`, `Resync`, `RescanImports`, `New`, `Agent`, `Compact`, `Theme`, `Streaming`, `Model`, `AuthPool`, `Provider`, `Context`, `Cwd`, `Worktree`, `Ralph`, `Goal`, `Skills`, `Skill`, `Thinking`, `Timeline`, `Stop`, `CancelRuntime`, `Runtime`.

### `PresentationAction`

`SubmitMessage`, `CancelTurn`, `UpdateDraft`, `ResolvePermission`, `ResolvePermissionBatch`, `MoveHistoryWindow`, `ResolveInteraction`.

### Top-level `Commands`

`Onboard`, `ArtifactId`, `Server`, `State`, `Session`, `Web`, `Plugin`, `Theme`, `Model`, `Auth`, `Login`, `Permission`, `Interaction`, `Worktree`, `Workflow`, `RuntimeWork`, `Cancel`, `Attach`, `Tui`, `Send`.

### Top-level CLI ownership classification

Every current top-level command family has one explicit primary owner. Subcommands may refine the classification only where noted in the detailed inventory.

| Command | Primary classification | Owner and boundary |
| --- | --- | --- |
| `Onboard` | Frontend user state / credential custody | Local onboarding and settings orchestration; secure imports remain auth/vault-owned and do not mutate canonical sessions implicitly. |
| `ArtifactId` | Offline/lifecycle | Local produced-artifact identity diagnostic. |
| `Server` | Offline/lifecycle | Daemon lifecycle coordination; status and live control cross the typed daemon boundary, while startup/retirement owns local process coordination. |
| `State` | Offline/lifecycle | `state locations [--json]` resolves configured locations and probes availability without creating or repairing state. `state prune-staging [--root ROOT] [--apply] [--json]` delegates interrupted-relocation staging inventory/cleanup to the session-migration owner; mutation requires `--apply`, and live staging is retained. Neither command is routine canonical session access. |
| `Session` | Shared application, with explicit maintenance subcommands | Routine lifecycle, bounded reads, configuration, search, import, and derivation are daemon-backed; doctor/repair/reindex/migration/reset/release diagnostics remain explicitly named maintenance. |
| `Web` | Frontend local | HyperChad renderer startup and bind policy; it consumes shared semantics without owning product behavior. |
| `Plugin` | Plugin development or shared application | List/check and non-daemon service execution are explicit offline plugin development; `--daemon` services/invoke/call/publish use the live plugin-host application boundary. |
| `Theme` | Offline local artifact validation | Lists, validates, or copies declarative theme sources; interactive TUI theme selection remains frontend user state. |
| `Model` | Shared application, with local declarative ignores | Catalog/status operations are daemon-backed; ignore-file management is explicitly local declarative state. |
| `Auth` | Credential custody / shared application status | Login/profile/vault mutations remain auth-owned credential custody; daemon-backed pool/status/usage operations use typed application contracts. |
| `Login` | Credential custody | Deprecated local compatibility adapter to `Auth` login ownership. |
| `Permission` | Shared application | Pending inspection/resolution and policy-rule creation use typed daemon operations; configuration persistence remains policy-owner behavior behind the operation. |
| `Interaction` | Shared application | Pending exchange inspection and schema-aware resolution use typed daemon operations; semantic-controller automation remains tracked work. |
| `Worktree` | Shared application | Daemon-owned worktree operations and canonical session attachment. |
| `Workflow` | Shared application, with local package validation | Runtime/authoring/import/export operations are daemon-backed; source-controlled package discovery/validation is explicit local artifact work. |
| `RuntimeWork` | Shared application | Bounded runtime-work inspection, watch, and cancellation through the daemon API. |
| `Cancel` | Shared application | Canonical turn cancellation through the daemon API. |
| `Attach` | Shared application / TUI adapter | Opens a canonical daemon session in the TUI without owning session semantics. |
| `Tui` | Frontend local adapter | Starts the terminal renderer over shared application semantics. |
| `Send` | Shared application | Canonical turn admission through the typed daemon client, including placement and idempotency metadata. |


* Add or update an inventory row whenever a shared semantic operation, renderer action, top-level CLI family, or ownership classification changes.
* An IPC request alone is not parity. Verify the typed client API, owner, authorization, cancellation, CLI behavior, output contract, and observable result.
* Do not classify presentation or maintenance as shared application behavior merely to claim parity.
* Do not expose raw IPC, persistence, TUI, terminal, provider-private, or plugin implementation types as application contracts.
* Test operation dispatch, authorization, and frontend outcomes directly; do not duplicate source inventories.
