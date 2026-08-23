# Application operation parity

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
| `session.select_agent` | `/agent`, plan/build aliases | Shared application; agent profile/session | `SessionViewAction::SetAgent`; typed client method | `session set-agent SESSION_ID AGENT_ID [--json]` | Agent policy and permissions remain authoritative. |
| `session.auth_pool` | auth-pool picker | Shared application; auth/session | typed client preference method | `session set-auth-pool POOL (--profile PROFILE\|--clear) [--json]` | Secret custody remains provider/plugin-owned. |
| `session.skills` | skills palette, activate/deactivate | Shared application; skill/session | `ActivateSkill`, `DeactivateSkill`, typed client methods | `session active-skills`, `activate-skill`, and `deactivate-skill`, each with `--json` | Skill identity is typed and plugin/domain-owned. |
| `permission.inspect` | permission list/surface | Shared application; permission | typed list method | `permission list [--session-id SESSION_ID] [--json]` | Canonical normalized operation facts are exposed without secrets; filtering is over the bounded pending set. |
| `permission.resolve` | permission surface | Shared application; permission | `ResolvePermission`, `ResolvePermissionBatch`; typed client methods | `permission approve ID [--remember] [--json]`, `permission deny ID [--json]`, `permission resolve-batch BATCH_ID (--approve\|--deny) [--json]` | Authorization decision precedes the blocked side effect; batch resolution is canonical and duplicate terminal resolution is safe. |
| `interaction.inspect` | interactive tool surface | Shared application; plugin/tool exchange | pending exchange client method | `interaction list [--json]` | Request includes producer, schema, version, response policy, and bounded opaque payload. |
| `interaction.drive` | rich TUI/HyperChad interaction | Shared application with plugin-owned semantics | HyperChad uses a plugin-owned `InteractionInput` controller locally; planned daemon-hosted controller contract still needs JSON snapshots and `InteractionOutput` | CLI gap | Preferred generic fallback. HyperChad validation/activate/change/submit behavior is integration-tested over real IPC, but dynamic daemon-hosted controller discovery and CLI access remain implementation work; unknown schemas are surfaced rather than guessed. |
| `interaction.resolve_raw` | generic exchange fallback | Shared application; plugin/tool exchange | typed `ToolExchangeResolution` client path | `interaction respond EXCHANGE_ID --payload FILE [--json]` and `interaction cancel EXCHANGE_ID [--json]` | Schema-aware fallback. JSON input is bounded to 256 KiB, `-` reads stdin, unknown versions remain producer-owned, and authorization/compatible-adapter checks precede resumption. |
| `worktree.list_create_remove` | `/worktree`, create/attach effects | Shared application; worktree | typed start/status/wait/create/remove methods | `worktree list`, `create`, and confirmed `remove`, with `--json` | Paths are confined; branch modes conflict explicitly; creation is addressable/idempotent; optional canonical session attachment/creation is domain-owned. |
| `workflow.author` | workflow control center/plugin surfaces | Shared application; workflow | typed authoring, validation, publication, and inspection methods | `workflow author` | Workflow contracts and persistence stay domain-owned; JSON is already common but must be inventoried per subcommand. |
| `workflow.execute` | workflow/Ralph actions | Shared application; workflow/runtime | typed start, inspect, input, approval, output, cancellation methods | `workflow start/inspect-run/run-output/provide-input/resolve-approval/cancel-computation` | Durable workflow authority, permission, idempotency, and cancellation rules apply. |
| `plugin.service` | plugin commands and surfaces | Shared application when targeting the running host; plugin | typed discover/call/invoke/publish methods | `plugin ... --daemon` | Schemas are versioned and plugin-owned. Local mode is explicitly development/diagnostic. |
| `agent.spawn` | delegated Bcode agent | Shared convenience operation composed from session/worktree/selection/turn owners | planned result: canonical session plus `TurnAdmission` | CLI gap; final naming follows CLI inventory | Must authorize before effects and define retry safety across session creation and turn admission. No subprocess or child-run store. |
| `session.watch` | live TUI/HyperChad observation | Shared application; session view/frontend contracts | normalized snapshot/live event APIs | `session watch --json` and `runtime-work watch --json` emit JSON Lines | Initial state is bounded; envelopes distinguish snapshot, durable, live, runtime-work, and resync-required records. Resync ends the stream for explicit reconnect/replacement; no durable resume is claimed. |

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
| `workflow.package_local` | source package discovery and local file validation | Source-controlled workflow package owner | Bounded workspace scanning and explicit typed submission for daemon mutations. |

## Automation examples

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

bcode interaction list --json
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

`Onboard`, `ArtifactId`, `Server`, `Session`, `Web`, `Plugin`, `Theme`, `Model`, `Auth`, `Login`, `Permission`, `Interaction`, `Worktree`, `Workflow`, `RuntimeWork`, `Cancel`, `Attach`, `Tui`, `Send`.

### Top-level CLI ownership classification

Every current top-level command family has one explicit primary owner. Subcommands may refine the classification only where noted in the detailed inventory.

| Command | Primary classification | Owner and boundary |
| --- | --- | --- |
| `Onboard` | Frontend user state / credential custody | Local onboarding and settings orchestration; secure imports remain auth/vault-owned and do not mutate canonical sessions implicitly. |
| `ArtifactId` | Offline/lifecycle | Local produced-artifact identity diagnostic. |
| `Server` | Offline/lifecycle | Daemon lifecycle coordination; status and live control cross the typed daemon boundary, while startup/retirement owns local process coordination. |
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
* Keep checked source inventories exhaustive through `scripts/check-application-operation-parity.sh`.
