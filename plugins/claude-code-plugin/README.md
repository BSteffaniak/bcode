# Claude Code managed harness

`ClaudeCodePlugin` is a native Rust plugin (`cdylib` and statically usable `rlib`).
Its manifest declares `bcode.harness/v2`; it never registers a model provider or
executes an observed native tool through Bcode's tool dispatcher.

## Integration

The coordinator must register workspace membership/dependencies for
`bcode_claude_code_plugin`, `bcode_claude_code`, and `bcode_harness_models`, and add
the plugin to the bundled inventory. Other dependencies are existing workspace
`bcode_plugin_sdk`, `bcode_provider_auth_models`, `serde`, `serde_json`, and `tokio`.
Static entry type: `bcode_claude_code_plugin::ClaudeCodePlugin`.
Dynamic entrypoints are emitted by `bcode_plugin_sdk::export_plugin!`.

Resolved plugin configuration is a typed `instances` map. Each instance has:

* `executable`: explicit absolute executable path.
* `config_directory`: explicit absolute `CLAUDE_CONFIG_DIR`; the incoming config
  namespace and auth credential namespace must both equal this selected path.
* `exact_version`: exact expected native `--version` output.
* `verified_session_state`: defaults false; only enable after testing this exact
  executable's native session-state/background-task behavior. This mode declares
  authoritative settlement `Supported`: the SDK itself requires idle plus a
  result and completed tracked background tasks before emitting settlement, with
  fresh idle required after task completion. Unknown task classes disable that
  evidence; unverified mode reports unknown activity, never settlement. These
  enforced producer guarantees are not free-text host admission constraints.
* `verified_native_auth_status`: defaults false; only enable after testing the exact
  executable's local `auth status` contract, namespace output, exit status, and
  native-account billing classification. Allows status preflight and runtime launch
  for an already logged-in account, but never login handoff. Authentication capability
  remains `Unsupported` with an explicit status-only constraint; local status is
  available, not remote credential verification. This does not verify settlement.
* `verified_native_auth`: defaults false; only enable after testing the exact
  executable's `auth status` namespace output and supported native login handoff.
  Retains the combined status-and-login attestation for backward compatibility and
  takes precedence over `verified_native_auth_status` when both are true.
* `setting_sources`: absent preserves native settings inheritance; a list selects
  explicit native sources (an empty list excludes optional sources). Managed
  policy is never bypassed.

Only explicitly selected native-account custody with auth provider ID `claude-code`
is implemented. Ambient API-key,
cloud, alternate endpoint, and token routes fail closed instead of silently
changing billing. The process receives a narrow non-auth environment allowlist.
No native credential files, keychains, or token stores are read.

The host must admit each operation under current canonical session ownership and
serialize mutations before dispatch. Plugin binding comparisons are additional
process-generation checks, not a replacement for canonical fencing. Normalized
observations must be persisted before the host advances the opaque page cursor.
Pages are bounded by item and wire size. Retained observation reads validate the
exact instance/target/generation independently of native process liveness: EOF
invalidates live callbacks but does not erase queued confirmation or interaction
evidence. Post-EOF pages report disconnected/unknown status and a gap; they do not
authorize responding, submitting, replaying, or reviving the transport. Native pipes are drained on a persistent
Tokio runtime independently of service polling. Queue overflow stops the process
and exposes a gap; unread history is never silently dropped.

`observe` honors `wait_millis`: when nothing is ready to offer, the plugin applies the
request's acknowledgement, absorbs native records that produce no observations, and holds
the page until evidence or a host request is ready, the wait elapses, or the invocation is
cancelled. The plugin is a concurrent native plugin: every operation still serializes on
retained state, but a held `observe` waits outside that lock, so submissions, responses,
interrupts and tool-queue drains are never queued behind it.

## Implemented semantics

* Typed routing for inspect/open/submit/observe/respond/interrupt/close/reconcile
  and all four auth operations.
* Exact session/instance/native binding checks and retained mutation correlations.
* Interrupt capability `Supported` means correlated native interrupt delivery on
  the exact host-owned, control-ready Prepared or Confirmed transport. It does not
  promise settlement, rollback, stopped background work, or absence of effects.
  The plugin returns `Acknowledged` only after the native control response; write
  completion, idle hints, and result observations cannot fabricate that response.
  Timeout/disconnect remains uncertain and cannot authorize replay. `Constrained`
  is reserved for actual capability prerequisites the host must explicitly verify,
  such as the parent-assistance grant—not generic limits on acknowledgement meaning.
* Native write completion remains `DeliveryAttempted`, not `NativeAccepted`.
  Missing state after restart reconciles as uncertain, never not-applied.
* Results remain nonterminal. Only verified native settlement is classified as
  settled. No quiet timeout becomes a successful execution result.
* Assistant text, native tool/subagent observations, single-question single-select
  clarification, native approval, callback cancellation and scoped MCP assistance
  normalization. Raw protocol/tool inputs remain transient and are not emitted.
* Native MCP callbacks are never promoted into parent assistance from observed
  names alone. Until a verified host parent scope is available, discovery exposes
  no tools and requests receive explicit unsupported protocol responses.
* Local native auth status (not remote verification), explicit `NativeHandoff`
  interaction, status reinspection on continuation, local-flow cancellation
  without logout or credential deletion. Sensitive auth input is rejected rather
  than sent as ordinary stream text.

## Integration gaps and constraints

* Uses library control `initialize()` without waiting for identity. New sessions
  select the retained Bcode UUID via `spawn_new_with_sdk_servers`; exact resume
  selects only the host-provided native ID. Open returns `Prepared` while the
  selection is merely requested, and `Opened` only for library-confirmed identity.
  The exact retained `HarnessSessionTarget::Prepared` permits one first submission
  on the same live transport. Native confirmation produces typed `BindingConfirmed`
  before normalized activity. Contradictory IDs terminate; process loss remains
  uncertain. No sentinel binding, dummy prompt, or automatic retry is used.
  Host must persist Prepared before submission and confirmation before target
  promotion. End-to-end create/resume behavior remains unvalidated.
* Public `native_login_handoff_data(instance, options)` returns a versioned,
  namespace-bound direct-execution plan. Manifest-declared plugin extension
  `bcode.claude-code.handoff/v1`, operation `prepare_login`, accepts
  `NativeLoginHandoffRequest` and returns `NativeLoginHandoffResponse` without
  starting a runtime or process. Lane 11 must route this plan to a user-authorized
  client terminal, verify exact version, clear ambient environment and use the
  plan's complete allowlisted environment, then reinspect native status.
  The shared `NativeHandoff` interaction still carries only a request ID; this
  extension supplies the concrete execution data. Never run it on daemon stdio.
  Native-auth provider contribution/registry routing remains coordinator-owned.
* Parent assistance has real SDK MCP protocol handling in `parent_bridge.rs`:
  initialize, tool discovery, exact `request_parent_assistance` calls, bounded
  single-question argument validation, and typed pending `Assistance` interactions.
  The normalizer retains the original MCP correlation, continues other observations
  while waiting, fences responses against the exact generation/current scope, and
  invalidates cancelled native callbacks. No arbitrary native tool dispatch exists.
  Open validates the shared `HarnessAssistanceGrant` against child identity and
  generation, retains the exact grant in the immutable open and every pending
  assistance call, and registers the SDK MCP server only when the admitted grant
  is present. Responses and cancellation require that exact retained grant,
  including admission ID and parent generation—not merely equal scope text.
  The host must issue/verify the grant against both current owners before open,
  reauthorize each response, and close the native process on grant revocation or
  owner change. The plugin cannot independently query canonical parent ownership.
  No plugin-config self-grant exists. No shared types were duplicated or edited.
  Source integration is implemented; native/end-to-end behavior is unvalidated.
* Active steering is unsupported: native stream input has no atomic expected-turn
  comparison. A next turn is admitted once the previous foreground result arrived
  with no queued native input and no pending callback; verified session-state mode
  additionally waits for the native idle marker. Foreground admission is not
  settlement: background work stays observed, and workflow completion still
  requires authoritative settlement evidence.
* Each observation page cycle pumps staged native records until the page budget is
  used or execution evidence needs host acknowledgement or hydration. Records
  without execution evidence do not each cost a host round trip.
* Mutation caches retain at most 256 recent entries per live process and rotate
  only at a verified settled boundary. Canonical host admission must deduplicate
  lifetime identities **before** dispatch, including after cache eviction/restart.
  A cache miss always reconciles uncertain, never not-applied. This integration
  requirement must be verified before enabling mutation routing.
* Session-state markers may lack a native turn identity. Such evidence is emitted
  as settled status plus an explicit diagnostic, never a fabricated turn ID.
* Inspection reports configured compatibility/provenance; spawning/auth status
  performs actual executable version verification. It is not a remote auth or
  executable digest verification.

## Submission attribution and remaining completion limits

The Rust transport retains a caller UUID for each exact submission attempt.
Verified native reply and result attribution produce `SubmissionConsumed` and
`SubmissionResultObserved` under the current session and ownership generation.
Merged native frames can name multiple retained attempts. These observations
prove attribution, not a native turn ID, settlement or replay safety. They do
not populate `SubmissionAccepted.native_turn_id`.

An authorized native Claude Code 2.1.308 test exercised the production Rust
library over two submissions on one live subscription-backed connection. Exact
UUID attribution, expected replies and native state observations were verified.
This is library acceptance, not full Bcode workflow or interactive acceptance.

A separate native background-shell probe observed result and idle before shell
completion and an automatic follow-up. The library therefore fences unsupported
background activity rather than completing from idle or queue zero. General
workflow completion still requires independently verified assignment-correlated
terminal and background-settlement evidence. No artificial identity prompt,
UUID-as-turn-ID substitution or automatic resend fills that gap.

The service boundary is `bcode.harness/v2`; old v1 plugins are not admitted as
compatible v2 producers. See the session persistence architecture for durable
schema/writer migration.

## Native login evidence and client-terminal integration

The `prepare_login` extension returns a direct-execution plan with typed `method`
evidence: fixed `native-login`, `Supported`, `NativeManaged`, `ClientTerminal`, and
`native_inactivity: unknown`. Support is gated by exact-version
`VerifiedStatusAndLogin` configuration and namespace checks, not inferred from
readiness. The added method object is additive v1 response data; consumers of
older responses lacking it must not infer method support. No global inactivity
proof is available from the native CLI status API. The plugin never enumerates
processes or reads credentials to invent one.

Host/client integration must distinguish a **fresh reserved terminal handoff**
from recovery of an ambiguous prior login. Before fresh execution the canonical
host must authorize the caller and reserve the actual credential namespace,
exclude competing managed logins across instances, and retain the reservation
through terminal child exit/reap and subsequent read-only status inspection.
The client verifies the exact executable version, clears ambient environment,
executes the fixed argv directly in its explicit terminal, and owns cancellation
and reap of that child. Native URLs/codes stay in that terminal, never daemon
stdio, session history, or model context. Exit alone is not readiness.

A reservation covers cooperating host-managed flows only, not arbitrary external
native processes. It cannot be represented as positive native-wide inactivity.
A daemon restart, lost terminal connection, or unresolved prior flow must remain
uncertain until the actual owned child is reconciled; `Ready`, an empty registry,
and cancellation of the plugin's bookkeeping are not process-liveness evidence.
If host admission requires global native inactivity even for a fresh flow, that
API prerequisite is unsatisfiable with the currently supported CLI. A generic
typed host-reservation/client-owned execution contract is required instead;
this plugin cannot relax admission or silently substitute that contract.

Behavioral tests are written in `src/tests.rs` and `src/capabilities.rs`. They have
not been compiled or executed. Runtime/package/plugin/architecture validation and
end-to-end acceptance remain pending.
