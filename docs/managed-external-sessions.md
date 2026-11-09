# Managed external session architecture

## Boundary and ownership

An external harness is an execution backend for an ordinary Bcode session, not a
model provider. Session parentage, workspace selection and canonical history retain
their existing ownership. Unrelated native conversations remain import-plugin scope.

The application boundary admits interactive, tool, command and workflow operations.
It resolves the configured harness instance and verifies execution ownership before
calling the native plugin. Selecting a session or naming a parent is not authority.
Plugins interpret native protocols and capabilities; they do not schedule Bcode tools
from observed native tool calls. Native subagents are observations unless a separate
application operation explicitly creates an independently controlled Bcode child.

Portable contracts live in `bcode_harness_models`, plugin registration/routing in the
plugin domains, and canonical external evidence in session models and persistence.
`bcode_claude_code` is a protocol/process library independent of Bcode's application,
persistence and workflow implementations. Claude Code and Codex integrations are
separately disableable plugins, as is the generic managed-session tool plugin.

## Identity and evidence

Do not collapse these identities:

* Bcode session and immutable parent association;
* configured harness instance and credential/configuration namespace;
* native conversation;
* submission, attempt, native turn/run and mutation;
* execution ownership generation and connection generation;
* outstanding interaction and its response.

Transport preparation and native identity confirmation are distinct. A successful
control initialization may retain a requested native UUID without proving a native
conversation exists. The first admitted submission can use that prepared live
transport; only matching native identity evidence promotes it to a confirmed
binding. Preparation never authorizes resume, replay after process loss, or an
artificial prompt to force identity discovery.

Execution, connection health, attention, continuation and observation completeness
are separate facts. An empty observation page is not settlement. Neither native
result delivery, interrupt acknowledgment, quiet time nor process exit independently
proves that background work has settled. Unknown native support remains unknown or
constrained; capability declarations must not infer support from a vendor name.

`SubmissionAccepted` records native proof linking an exact submission and attempt
to a native turn, under the enclosing observation's session and generation. Request
acknowledgments cannot supply that mapping. Codex supplies genuine native turn
identities; Claude's prompt/result UUIDs identify native frames, not native turns.
The legacy strict settlement path retains its stronger correlation requirements.

### Separated lifecycle contract

The `bcode.harness/v3` interface adds `Execution` observations without changing the
schema-one request envelope. Schema 55 / writer epoch 20 fences their durable
representation; earlier history is converted by session-migration and does not
acquire invented lifecycle evidence.

* `SubmissionTerminal` retains the exact Bcode submission and attempt, native
  terminal evidence, disposition, and complete output when available. A genuine
  result frame suffices structurally without fabricating a native turn identity.
* `BackgroundWork` retains adapter-owned work identity, optional genuine native
  task identity and exact origin, state, parent relationship, and notification
  status. Completed work can still owe a completion notification.
* `Continuation` identifies a distinct automatic native follow-up and its causal
  origin. It cannot reopen the terminal foreground attempt or authorize Bcode
  worker dispatch. Native turn identity remains optional, never synthesized.
* `Coverage` describes supported tracking through the current observation. It is
  not a no-future-work certificate, does not erase retained work or notifications,
  and cannot make an unobserved native activity class authoritative.

Foreground turn admission is separate from settlement. An adapter may admit the
next interactive turn once the previous foreground result arrived with no queued
native input or pending callback, even when settlement support is unknown. That
admission never satisfies a completion boundary below and never erases retained
background work.

Callers choose a completion boundary: foreground, associated work (the default),
or workspace release. Foreground requires authoritative exact-attempt outcome and
complete attributed output, not universal quiescence. Associated work additionally
requires complete relevant tracking, resolved background work, notifications and
required continuations. Uncorrelated outstanding work fences session-wide safety;
unknown tracking is not an empty authoritative roster. Workspace release additionally
requires host-owned release authority; coverage alone cannot establish it.
Background failure can fail an enclosing assignment while its foreground success
remains unchanged. Native continuation execution and explicit asynchronous handoff
must retain responsibility separately; neither may be inferred from idle or a
successful response.

These are compatibility and implementation contracts, not a claim of completed
native, workflow, or frontend acceptance. Each adapter's supported tracking and the
production persistence/policy journey require separate verification.

## Canonical delivery and recovery

The session writer persists ownership-fenced mutation intent before native delivery.
An identical retry returns recorded evidence when available. Conflicting reuse of an
identity fails. If delivery or acknowledgment is ambiguous, retain uncertainty and
reconcile; never repeat the native mutation merely because a client disconnected,
a timeout elapsed or a daemon restarted.

Normalized observations are appended/deduplicated through the canonical session
writer before publication. Projections and cursors are disposable derived data, not
a plugin-owned recovery journal. Routine history and current-state access are bounded
and indexed. Missing or incompatible projections report degraded/maintenance state;
normal attach and catalog paths do not silently replay or repair history.

Native plugin availability is independent of stored-history availability. Disabling
a plugin or losing a native executable must not erase canonical evidence or prevent
unrelated sessions from opening. Resuming native work requires current ownership,
compatible exact native identity and supported reconciliation semantics.

## Interactions and cancellation

Questions, approvals and requests for assistance have different authority semantics.
Responses bind the exact pending interaction, admitted caller, native binding and
ownership generation. Stale, mismatched or already-consumed responses fail closed.
A native request is not itself permission to execute a Bcode operation. Parent
assistance requires separately admitted authority rather than inherited parentage.

### Delegated control from a parent session

The only parent-relative control is *delegated control*: the active, host-registered tool
invocation of a child's immutable canonical parent session may address that existing managed
child. Naming a parent in a request is still not authority; the authenticated invocation's own
session must be the child's recorded parent, the child must already be a managed session in the
same canonical workspace, and every operation still passes tool permission policy and canonical
ownership, generation and retained-target admission. Creation remains limited to the calling
session as parent. Clients and workflow callers keep their own admission routes.

Two invocation-only reads support delegation (`bcode_harness_models::delegation`):
`binding` returns the child's exact retained instance and prepared/confirmed target (or none
when no session with that identity exists yet), and `history` drains at most one bounded native
observation page with the committed acknowledgement cursor before returning one bounded
canonical history page. Neither grants mutation, replay, resume or transport authority.

When the native harness withdraws an outstanding request (for example Claude Code cancels a
pending `can_use_tool` callback after an interrupt, or the native connection is lost or closed),
the plugin records an `InteractionWithdrawn { request_id }` observation. The canonical
interaction locator then names the withdrawal, so a late response is refused before dispatch,
and frontends close the prompt. A withdrawal is neither an answer nor a Bcode decision.

Cancellation retains its canonical intent and dispatch receipt. Native interruption
is an attempt whose acknowledgment is not authoritative settlement. Unconfirmed
effects remain visible as uncertain; workflow completion requires authoritative
execution evidence, not merely a session identifier or a native result message.

## Authentication and billing

Provider-auth owns custody and namespace-level coordination. Supported custody modes
are native-managed, vault-managed and explicitly selected external custody. A native
account must not be represented by an empty vault profile. Secret access remains
plugin-scoped; account selection must not silently fall back to a different billing
route.

Daemon-owned sensitive flows are ephemeral and bounded, shared only by the actual
credential namespace with independently cancellable waiters. Reconnect may recover
a still-live flow; restart requires fresh readiness inspection, not replay of a
callback or login mutation. Sensitive URLs, codes and input never enter ordinary
session history, model context, tool traces or workflow payloads. Readiness status,
refresh and remote verification are distinct operations.

Plugins may explicitly declare the query service `bcode.harness.terminal-auth/v1`
(`TERMINAL_AUTH_INTERFACE_ID`). Its `prepare_login` operation accepts the existing
schema-one `{schema_version, request_id, instance}` request and returns
`{schema_version, request_id, handoff}`, using the existing verified native login plan.
The owning plugin interprets configuration; the host validates correlation, namespace,
executable, custody and current admission before issuing the plan.

`ManagedSessionCommandHandle::terminal_auth_interface()` returns `Option<String>`
from the enabled service metadata for the canonically bound plugin only. Exactly one
matching declaration is required; duplicates fail closed and absent declarations return
`None`. Discovery grants neither readiness nor permission and does not launch native
programs. Callers must explicitly yield terminal ownership and obtain consent before
`ManagedSessionAuthFlow::terminal_handoff(interface_id)`. Reaping the exact child and
host reinspection remain mandatory; successful exit is not authentication readiness.
Claude additionally retains `bcode.claude-code.handoff/v1` for existing explicit clients;
generic discovery never derives or falls back to a vendor-specific interface.

Claude configuration selection consistently scopes status, login, execution and
resume through the same native configuration directory. Native login handoff belongs
on an explicitly connected client terminal, never daemon-inherited standard input.
Codex uses app-server and expected-turn steering. API-key delivery requires verified
ephemeral native credential storage; persistent fallback is not equivalent.

## Live ingestion

Interactive clients never poll for native output. An attached client issues one `watch`
request; the daemon then drives the attachment poll for that session, asking the adapter to
hold each page (`wait_millis`) until native output arrives. Each committed page reaches
clients through the ordinary session event subscription. There is one watch per session,
shared by every attached client. Requests may arrive on short-lived connections, so every
page is re-admitted as an interactive client attached to the session (falling back to the
requesting client while it is connected); the watch ends when no such client remains, the
owner generation changes or the daemon shuts down. Each page runs under the configuration
view pinned for its owner, exactly like admitted execute calls, never the daemon's startup
baseline (which may predate the instance's setup). Adapters that
ignore `wait_millis` remain correct: empty pages that were not held back off. A page that
carries live updates counts as progress, so streaming text asks for the next page immediately;
the adapter's own coalescing window bounds the page rate. The Claude Code plugin holds a
live-only page for its whole coalescing window (40 ms from the first ready delta), returning
early only for durable evidence or when the live buffer reaches half of its bounds.

Long holds occupy a shared plugin invocation slot, so only a bounded number of sessions hold
at once; others use short pages with backoff. Explicit `observe` callers that pass their own
cursor are unaffected; a watch is attachment-scoped, not an always-on ingester.

## Presentation

SessionView owns renderer-neutral external-session semantics. Clients render
canonical observations even when the execution plugin is unavailable. Native text
is untrusted display content, not browser markup, terminal authority or tool calls.
Frontend response controls require current capabilities and exact interaction
correlation. Sensitive auth surfaces remain separate from the ordinary chat composer.
BMUX remains the terminal layout authority.

### Native-like transcript projection

Both SessionView reducers (`apply_event` and `apply_event_bounded`) project each canonical
external event into ordinary transcript items instead of one literal status message per event
(`bcode_session_view::external_projection`):

| Canonical evidence | SessionView item |
| --- | --- |
| `DeliveryClaimed` with a `Submit` request, or a legacy submission record | User message (`external-submission:{generation}:{submission}`) |
| `Text` | Assistant markdown labelled with the harness name; segmented text uses `harness_stream_identity(..).assistant_item_id()` so it replaces the live stream in place |
| `Reasoning` | Completed reasoning activity at `reasoning_item_id()`, honoring thinking visibility; opaque reasoning shows as withheld |
| `Tool` / `ToolResult` | Tool invocation `external:{generation}:{native_tool_id}` with the observing plugin as producer, input JSON as arguments, then finished/failed with literal output |
| `Usage` | Session usage (`runtime.latest_usage`, cost, cumulative tokens) keyed `external:{generation}:{observation}`, folded into the daemon usage projection with its page; `total_cost_micros` is the increment for that result (the Claude Code plugin converts Claude's running process total), so entries add. A reported cost is the vendor amount and is never repriced; an unreported cost (subscription) counts tokens with no estimate |
| `Result` | Nothing when it repeats the latest assistant text, otherwise an assistant message |
| `Interaction` | Compact notice plus `SessionViewSnapshot.pending_external_interactions` |
| `InteractionWithdrawn` | No item; removes the matching pending interaction |
| `Diagnostic`, failures, uncertain or not-applied reconciliation | Compact friendly notice |
| Execution, submission, binding, status, settlement, terminal, vendor, receipts, acknowledgements, preparation, process release, recovery | No item; execution lifecycle only |

`Diagnostic` is reserved for conditions a user should see. Routine operational telemetry, such as
the Claude Code plugin's closed-vocabulary host-tool bridge status (namespace
`claude_code.host_tools`) and uncorrelated settlement evidence (namespace
`claude_code.settlement`), is retained as `Vendor` evidence so canonical history keeps it without
adding transcript notices to every native turn. Process preparation, release and recovery are
likewise plumbing: a released conversation resumes transparently when reopened, so the
transcript reads like a native Bcode conversation.

The harness label is derived generically from the instance plugin id (`bcode.claude-code`
becomes `Claude Code`) and also labels live assistant stream items once known. Pending
interactions are removed by a successful `Respond` completion, a response record or an
`InteractionWithdrawn` observation, kept after a failed response, and cleared when a newer
ownership generation is observed. Observed native
tools never enter Bcode tool, permission or dispatch state, and terminal plugin visuals apply only
to tools owned by the declaring plugin. The bounded reducer journals every touched owner and
charges their exact before/after usage, restoring the view and ledger when the budget rejects
an event. The literal fallback (`canonical_external_text`, `external_history_page`,
`external_fallback_blocks`) is unchanged.

Live publishers must use `harness_stream_identity` for both streams. Reasoning live updates use
`ReasoningContentKind::Legacy` and `ReasoningContentRole::Unknown`, matching the durable
projection so display filtering is identical.

### Explicit Claude API-key custody

Claude native-account login remains the default (`billing.route = "native"`). API billing is a
separate, explicit route; it never imports native subscription credentials or falls back to them.
The API route requires a `VaultManaged` instance and an exact plugin-owned `sshenv` auth profile.
No key belongs in plugin configuration, workflow JSON, instance catalogs, or command arguments.

Example non-secret configuration (replace paths and verify the installed native version):

```toml
[auth.profiles.claude-api]
backend = "sshenv"
provider_id = "claude-code"
owner_plugin_id = "bcode.claude-code"
scheme = "api_key"

[auth.profiles.claude-api.map.api_key]
key = "ANTHROPIC_API_KEY"

[auth.profiles.claude-api.settings]
vault = "/absolute/path/to/auth-vault"
profile = "stored-api-account"

[plugins.config."bcode.claude-code".instances.api]
executable = "/absolute/path/to/claude"
config_directory = "/absolute/path/to/isolated-claude-config"
exact_version = "<exact installed --version output>"
setting_sources = []

[plugins.config."bcode.claude-code".instances.api.billing]
route = "api_key"
provider_id = "claude-code"
profile = "claude-api"
credential_namespace = '["/absolute/path/to/auth-vault","stored-api-account"]'
```

Enroll the API key through the existing provider-auth secret-field/vault enrollment surface owned
by `bcode.claude-code`; this example contains no secret and does not create a profile or grant.
The generic configured harness instance must use the same instance ID, executable and config
namespace, and this auth binding:

```json
{"provider_id":"claude-code","profile":"claude-api","credential_namespace":"[\"/absolute/path/to/auth-vault\",\"stored-api-account\"]","custody":"vault_managed"}
```

The credential namespace is the compact JSON array of the absolute vault path and actual vault
storage-profile name, not a display label. The profile's declared credential mapping must match
the owning plugin's registered API-key field. Current caller/workflow authorization and canonical
managed-session ownership are rechecked for every credential lookup. Matching configuration alone
never grants access. Missing, changed or revoked ownership, wrong custody/provider/profile/namespace,
unavailable material and cancellation fail closed. Ambient API keys and alternate native billing
routes remain rejected rather than silently selecting another account.

Local API readiness means that exact credential material is available for the selected launch;
it is not remote verification. API launch does not execute native-account `auth status` or login.
The key is injected only into the explicitly selected API process's cleared environment. Native
account status/login continues to use the native configuration namespace unchanged.

The plugin declares `bcode.plugin.sensitive-credentials/v1`. Its invocation-only callback carries
bounded non-secret selection metadata and returns raw secret bytes outside ordinary config,
bridge-response serialization, events and diagnostics. Unknown/older loaders reject the callback;
API execution is unavailable on those hosts, never downgraded to native-account billing. This
capability does not change the native callback ABI or the managed-session IPC schema.

## Compatibility and verification

The independently evolving service interface is `bcode.harness/v3`; older plugins
are not admitted through the v3 registry. The envelope shape remains schema one.
V2 introduced native-turn acceptance, consumed-prompt attribution, result
attribution, terminal disposition and background-work settlement. V3 additionally
separates foreground outcomes, retained work, notifications, continuations and
tracking coverage; none substitutes for another. Operation
requests and responses validate correlation, operation family, instance, generation
and bounded payloads. Durable session-event changes follow the existing migration
and writer-epoch mechanisms rather than a parallel event database. Event schema
55 / writer epoch 20 fences the new closed observation variants and indexes;
existing epoch-19 data requires the explicit registered migration, preserving the
previous epoch-18 migration chain.

Deterministic protocol fixtures and isolated application tests supplement, but do
not replace, exact-version native acceptance. Native login behavior, background
continuation, interruption and process-loss reconciliation require explicit release
evidence. Unavailable executables, missing accounts and unapproved spending remain
reported acceptance blockers; fixture success does not establish live compatibility.

### Opt-in native CLI acceptance

With explicit authorization to use the existing native Claude subscription, run:

```sh
cargo test --locked -p bcode --features app,static-bundled-claude-code-plugin \
  --test harness_native_acceptance -- --ignored --nocapture
```

The test verifies the pinned Claude executable and wrapped executable hashes, uses
native credential custody, and starts a matching client/daemon in private Bcode
state without any model-provider credentials. It checks two sequential submissions,
exact result/attempt correlation, foreground completion and settled activity, then
compares canonical history after daemon restart with the adapter disabled. It does
not establish native login handoff, safe recovery, managed-tool execution, or
background-continuation settlement. `BCODE_NATIVE_CLAUDE_CONFIG_DIR` may select an
existing native credential namespace; credentials must not be copied into test state.

For Codex (installed `codex` on `PATH`, existing native login in `BCODE_NATIVE_CODEX_HOME`,
`$CODEX_HOME` or `~/.codex`):

```sh
cargo test -p bcode --features app,static-bundled-codex-harness-plugin \
  --test harness_codex_acceptance -- --ignored --nocapture
```

It walks a selected-model open, a turn with a native approval, a later `NextTurn` after observed
completion, a mid-session model switch, a graceful daemon restart, recovery of the exact native
thread and resumed native context. Codex never reports authoritative settlement, so turns end at
the exact attempt's `SubmissionTerminal`. See `plugins/codex-harness-plugin/README.md`.

Observation clients must use the exact returned `next_after`, including `None` on
an empty page, and adopt confirmed session bindings. Canonical history has a
separate cursor: an empty page with an unchanged cursor is its current tail, not
proof that future events cannot arrive. Diagnostics emitted on recurring bridge
operations have distinct observation identities; bridge identity is not an event ID.

## Native-parity daemon behavior

### Live text publication

An observation page may carry a transient `live` sidecar of contiguous appends to native
assistant text or reasoning segments. `ManagedHarnessRuntime::observe` strips it before the
canonical commit and, only after that commit succeeds, the canonical store republishes it as
`SessionLiveEventKind::AssistantTextStreamUpdated` / `AssistantReasoningTextStreamUpdated`
keyed by `harness_stream_identity(generation, segment_id)`. Stream revisions are byte
positions (`offset + 1 ..= offset + len`), so contiguous appends from successive pages have
contiguous revisions and any gap or overlap is detected by consumers instead of guessed.
Live updates for a segment that the same page finalizes durably are dropped, and a durable
`Text`/`Reasoning` observation with a segment identity retires the actor's live checkpoint for
that stream (with a tombstone), so late attachers never see finalized text twice. Streams that
never receive a durable segment (an interrupted or aborted block, or a lost connection) are
retired, with tombstones, for the whole generation once a committed page shows the foreground
native turn ended (`Result`, `NativeTurnTerminal`, `Settlement`), a gap or a disconnected status,
or the transport is gone (`Close` completion, `TransportReleased`, `RecoveryPrepared`). The
Claude Code plugin truncates oversized assistant text instead of dropping the message, so its
segment still finalizes.

### Mid-session configuration

`ManagedSessionOperation::Configure` (CLI: `bcode harness configure --request`) is an
interactive client control. Workflow and tool invocations are refused before admission. The
daemon admits the current client and owner, requires the selected backend's declared
`model_switch` or `permission_mode` capability (`Supported` or `Constrained`), retains a
`DeliveryClaimed` record, delivers once and records `DeliveryCompleted`, exactly like
interruption. An identical retry returns retained evidence; an ambiguous delivery remains
uncertain and is never replayed. Configuration neither issues nor revokes managed tool grants.

### Restart and crash recovery

Recovery is published behind the native-parity storage fence (schema 58, writer epoch 26).
A clean daemon stop closes each owned transport and, when the selected plugin proves the
exact physical reap, appends `TransportReleased`. The canonical validator requires the same
owner's acknowledged `Close` of the current confirmed binding; afterwards that owner can write
nothing further. `harness recover` on a new daemon appends `RecoveryPrepared` only when the
canonical tail is that release (exact sequence and evidence), the generation advances by one,
the daemon differs, the old execution lifetime is released, the caller is an interactive
client, the session is not workflow-owned, the workspace, instance and capabilities are
unchanged, the plugin is enabled and its exact resume preflight passes. The retained resume
`Open` is then the only native creation the new generation may claim. Prior input is never
replayed.

A termination or hangup signal (for example system shutdown or logout) is handled like an
explicit stop, so the release above is recorded.

A daemon that ends without releasing (killed, crashed, power loss) leaves no
`TransportReleased`. Its native children are stdio processes that end with it. Recovery is then
admitted as an *abandoned* transport: `RecoveryPrepared` carries `abandoned = true` and
reconstructed end evidence (the dead daemon's execution lifetime, with
`ABANDONED_TRANSPORT_EVIDENCE` markers in place of a Close and reap identity). The recovering
daemon must find the previous daemon's published execution lifetime released for the same state
location (by instance identity; identities are never reused) and no live or unverifiable
session-owner lease for it; the canonical validator requires that no release exists for the
previous generation and that the recovery follows the exact canonical tail written by that
owner. Everything else is as above. Deliveries that were claimed but never completed remain
uncertain and are never replayed; the resumed native conversation keeps its own history.

Only observation pages move the current page locator and only binding deliveries move the
current binding locator, so a recovered generation that is released again before it commits a
page leaves those locators owned by an older generation. Any earlier generation of the same
session whose locator row precedes the current `RecoveryPrepared` transfer is released history:
the recovered generation starts with no retained predecessor, and consecutive recoveries remain
usable.

### Idle shutdown

The idle-only stop and idle timer are blocked while any managed session's committed evidence
shows live native work: a submitted foreground turn without a terminal result, unsettled
authoritative settlement, or nonterminal background work or continuations. Idle managed
sessions may still stop; they remain recoverable.

## Selecting a backend from an open conversation

`/model` opens the execution picker from ordinary and managed conversations. It lists
provider and configured native models with their backend/account identity. The picker's
setup/sign-in action uses the same native installation review and login journey as startup;
it remains reachable while discovery is pending or fails. Discovery never sends a test prompt.

Selection uses the central execution resolver. Provider model changes retain ordinary
sessions; native model changes on the exact attached instance use native configuration.
Crossing backends creates a new conversation in the current workspace without copying
history or changing the startup default. The previous conversation remains in Sessions.
Selection is unavailable until the current turn and pending native interactions finish.
Cancellation and errors return to the originating conversation without provider fallback;
uncertain starts must be inspected in Sessions before another creation attempt.

## Declarative configuration and local native registrations

Native installations can be declared in `bcode.toml` or added interactively through
Setup. Interactive setup never rewrites declarative configuration, including Nix-managed
symlinks. Its version-one registration document lives at
`<state-root>/plugins/bcode.harness/native-registrations.json`, independently of daemon
artifact caches. Credentials and login sessions remain owned by the native agent.

The generic harness plugin resolves declarations and local registrations together. A declared
instance ID wins as a whole, including disabled declarations; adapter settings are not merged
field-by-field across different installations. Differing local registrations are reported as
shadowed. Explicit plugin disables cannot be overridden by local registrations. A local entry
may be promoted into declarative configuration using the same ID without creating duplicate
execution choices. Adding an installation does not change the startup model default.

Registration uses a reviewed, freshly re-prepared native candidate and an optimistic state
revision. Atomic replacement under a filesystem lock prevents lost updates; unsupported or
corrupt documents are preserved and fail closed within native discovery/setup. Normal provider
operation does not depend on native registration health. Source verification can retain exact
loader-selected symlink targets for reading without granting authority to edit those targets;
a changed target requires a new launch/review. Active native sessions retain their exact admitted
configuration rather than switching account or executable after a configuration change.

Setup reuses a matching effective installation. Existing native login is inspected rather than
persisted as readiness; sign-in uses the agent's own authentication flow. Cancelling before
registration confirmation writes nothing. Cancelling login after registration leaves a registered
installation whose authentication can be completed later. F2 remains available while model
discovery is pending or fails; discovered unregistered tools are identified in picker status.

## TUI managed chat parity

The TUI presents a managed conversation in the same root UI as a native Bcode chat. The
policy lives in `packages/tui/src/managed_controls.rs`; transitions in
`packages/tui/src/managed_flow.rs`.

* **Native dialogs.** Pending interactions come from
  `SessionViewSnapshot::pending_external_interactions` for the attached generation only.
  Approvals use the existing permission dialog (approve/deny, never remember or batch) with
  the native `tool_name` and `input_preview` as untrusted display arguments. Questions and
  assistance requests use the question-plugin inline surface opened without canonical
  tool-exchange semantics (single radio question whose option values are native choice ids,
  free text allowed; assistance is free text only). A decision emits
  `TuiEffect::RespondManagedInteraction`, executed only through the sealed
  `ManagedSessionCommandHandle::managed_session_respond`. One response identity is minted
  per decision and reused while an outcome is uncertain; nothing is re-sent automatically.
  Dismissing a question sends no response; `/permissions` presents held requests again and
  `/native respond <generation> <request> <response> approve|deny|answer <choice> [text]|assist <text>|<JSON>`
  remains the text fallback.
* **Host operations.** Purely local presentation state (theme, profile, streaming policy,
  configuration reload) runs without any client. Allow-listed host operations (session
  picker/catalog, search and indexing, rename/delete, monitor/children, timeline,
  X-ray, command palette loading, plugin status notes) use a lazily constructed host-only
  client. Submission, interruption, responses, configuration and model execution never use
  it. Opening another session or starting a new one ends the managed root and continues in
  the ordinary route, which reroutes managed sessions back to managed reattachment.
* **Native settings.** `/model` and `/models` open the shared execution picker. Choosing
  another advertised model on the exact attached instance switches through
  `managed_session_configure` with `HarnessSessionSetting::Model` (the native selector);
  choosing another backend opens a new conversation. `/plan` selects
  `PermissionMode::Plan`, `/build` selects `Default`, `/agent plan|build|accept-edits`
  selects explicitly, and the agent-cycle key cycles Default → AcceptEdits → Plan. Each
  change uses a fresh mutation identity; at most one is in flight and ambiguous outcomes
  are reported, never replayed. Acknowledged settings appear in the status indicator.
* **Native slash passthrough.** Only `MANAGED_NATIVE_PASSTHROUGH_COMMANDS` (currently
  `/compact`) is submitted to the native agent as ordinary managed input. Other unknown
  slash commands are never forwarded; `/clear` is intentionally excluded. Slash
  completions in a managed chat are derived from the same allow-list.
* **Seamless reopen.** When the attachment watch reports `managed_session_stale_owner`
  (for example after a daemon restart), the root stops and the journey calls
  `ManagedSessionCommandHandle::recover` with a fresh recovery identity, then reattaches
  at the returned generation with a status message. Other continuity loss reattaches the
  same owner. Automatic continuation is bounded (three attempts per minute); on failure the
  TUI shows an actionable message naming `bcode tui <id> --managed-instance <instance>
  --managed-generation <generation>`. No submission, Start or native process is replayed.
* **Routing.** An ordinary open (`bcode tui <id>`, the session picker, search navigation)
  first reads the managed binding; a managed session is handed to the managed reattach
  route instead of an ordinary model attachment.
