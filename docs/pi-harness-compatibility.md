# Pi managed-harness compatibility and upstream auth RPC proposal

## Evidence and scope

This is a source-backed compatibility assessment and **proposal**, not an
implemented Pi authentication protocol. No Pi login, installation, live agent
request, or upstream publication was performed. The sibling repository was not
modified.

Evidence is pinned to Pi commit
[`6c384aa30f619d7b28963f2bab24d8dacbe73cb6`](https://github.com/earendil-works/pi/tree/6c384aa30f619d7b28963f2bab24d8dacbe73cb6).
The local `/hdd/GitHub/pi` checkout HEAD was newer
(`ad702637fdf6fbd3f965c5dba0b14bae100b74eb`); the assessment below uses `git show`
of the pinned revision, not an assumption that the working tree matches it.
Paths below are relative to that revision:

* `packages/coding-agent/src/modes/rpc/rpc-types.ts`: actual `RpcCommand`,
  `RpcResponse`, `RpcExtensionUIRequest`, and `RpcExtensionUIResponse` unions.
* `packages/coding-agent/src/modes/rpc/rpc-mode.ts`: command dispatch, prompt
  preflight acknowledgement, UI request correlation, and response routing.
* `packages/coding-agent/src/core/agent-session.ts`: `PromptDisposition`,
  `QueuedInputDisposition`, `AgentSessionEvent`, and `_emitAgentSettled`.
* `packages/coding-agent/src/core/model-runtime.ts`: `ModelRuntime.login`,
  `logout`, credential-operation serialization and credential synchronization.
* `packages/ai/src/auth/types.ts`: `AuthType`, `AuthInteraction`, `AuthPrompt`,
  `AuthEvent`, `LoginOptions`, and provider auth interfaces.
* `packages/coding-agent/src/modes/interactive/interactive-mode.ts`: `/login`
  dispatch and native login UI, separate from RPC prompt handling.
* `packages/coding-agent/src/cli/auth-command.ts`, `cli/auth-check.ts`: auth
  command parsing, non-refresh readiness inspection, secret-output opt-in.
* `packages/coding-agent/src/config.ts`: `getAgentDir`, `getAuthPath`, settings,
  models, session paths, and the `PI_CODING_AGENT_DIR` namespace override.

## Available behavior and honest capability mapping

External Pi is a **session execution backend**, not a Bcode model provider.
Bcode's canonical session owns lineage, delivery intent, native binding,
normalized observations, interaction generation, and reconciliation evidence.
Pi remains responsible for its native protocol, account/config semantics and
native execution. Observed Pi tool calls never dispatch a Bcode tool.

| Managed capability | Pinned Pi surface | Mapping / constraint |
| --- | --- | --- |
| Native submit | `prompt` with optional `id`, `message`, `images`, `streamingBehavior` | Correlate request IDs. Successful response carries `data.disposition`, not execution completion. `started`, `queued`, and `handled` differ. |
| Active input | `steer`, `follow_up` | Both return `queued` or `handled`. No `expectedTurnId` compare-and-set in the native command. Advertise steering constrained; do not claim native atomic expected-turn fencing. Reject unsupported strict placement rather than silently queueing next-turn input. |
| Interrupt | `abort` | A successful command acknowledgement does not alone establish settlement of all work or rollback. Lost acknowledgement remains uncertain. |
| Session control | `new_session`, `switch_session`, `get_session_stats`, `get_messages`, `get_state` | Native session identity is not an ordinary Bcode child. Managed open/resume must retain a verified native binding. Do not import/adopt arbitrary native sessions implicitly. |
| Settlement | `agent_end` versus `agent_settled { aborted }` | `agent_end` is a foreground event. `agent_settled` is the exposed automatic-run boundary. Keep reading continuously and use supported native settlement evidence, not stream silence. |
| Handled input | `prompt` response disposition `handled` | RPC client explicitly warns no run started, so there may be no later `agent_settled` to await. Do not invent a turn or hang waiting for one. |
| User questions | `extension_ui_request` select/input/editor with request ID; `extension_ui_response` value/cancelled | Normalize bounded questions with exact correlation and generation. Native UI support is extension-dependent, not universal built-in questioning. |
| Consent | `extension_ui_request` confirm; `extension_ui_response` confirmed | UI confirmation is not comprehensive native-tool security approval. Pi has no comprehensive built-in permission boundary comparable to an approval-enforced harness. Report security-approval support unsupported unless a separately audited extension actually supplies it. |
| Parent assistance | No generic authenticated Bcode-parent bridge in RPC | Unsupported unless a configured, explicitly authorized integration supplies a route. Never turn untrusted extension text into cross-session authority. |
| Auth inspect | `pi auth check --provider <id> --json --no-refresh` (or explicit model selection) | Run in the exact managed executable/config namespace. Omit `--credentials`; checks refresh by default without `--no-refresh`. Sanitize even inspect diagnostics. Readiness is not a fresh remote verification. |
| Interactive native login | Interactive-mode `/login` menu | Native handoff only in the present protocol. RPC `prompt` containing `/login` is **not** login and must not be used as a workaround. |
| RPC login | No provider-login command in `RpcCommand` | Structured login unsupported today. Proposed commands below must be capability/version gated; never probe by submitting secret-bearing text. |
| Auth custody | Pi `ModelRuntime` / selected agent-dir credential storage | Native custody, not host-vault custody by assertion. `PI_CODING_AGENT_DIR` scopes auth plus other agent files; isolate deliberately and preserve selected inheritance policy. Do not scrape `auth.json` or export credentials to emulate login. |

Native `get_state` is not a replacement for canonical Bcode history and cannot
prove a lost prompt had no effects. Same retained mutation ID after an ambiguous
write must return durable evidence or uncertainty; a fresh native request ID is
not permission to replay the business operation. Reconnect may observe or
reconcile but not resubmit an uncertain prompt/response/interrupt automatically.

The pinned `_emitAgentSettled` emits to extensions before emitting the client
event; deferred actions run afterward in its `finally` path. Settlement therefore
must be associated with the observed native run and managed submission, not used
to assert that the entire long-lived process can never do more work. Keep later
native work observable and do not retroactively reopen a canonical terminal
submission from a stale frame. Capability tests must cover extension-driven
follow-up behavior and explicit queue/run correlations.

## Proposed upstream protocol extension (not current functionality)

### Reuse the actual runtime contract

`ModelRuntime.login(providerId: string, type: AuthType,
interaction: AuthInteraction, options?: LoginOptions): Promise<Credential>` is
the right entry point. It creates an operation signal, serializes credential
operations for the provider, calls `this.models.login`, then awaits
`synchronizeCredentialState(providerId, "login", credential, signal)` before
returning. An RPC handler must call this method rather than bypassing it with
provider-specific OAuth logic or writing native credential files itself.
**Never serialize the returned `Credential` into RPC output.**

`AuthInteraction` is `{ signal?, prompt(prompt): Promise<string>, notify(event):
void }`. These are the real callbacks at the pinned revision; the legacy
`onAuth/onPrompt/onManualCodeInput` OAuth compatibility interface is not the
current `ModelRuntime.login` API.

`AuthPrompt` variants are `text`, `secret`, `select` (stable option IDs), and
`manual_code`, each optionally carrying its own abort signal. `AuthEvent`
variants are `info` (optional links), `auth_url`, `device_code` (code, verification
URI, optional interval/expiry and browser/clipboard hints), and `progress`.
This is enough to expose a headless login without routing anything through an
LLM prompt or the ordinary extension UI channel.

### Wire contract sketch

Introduce a negotiated `auth_rpc_v1` capability with declared auth methods and
limits. These names and fields are proposed, not accepted commands today:

```ts
// All begin/continue/cancel commands have an ordinary RPC correlation id.
{ id, type: "auth_begin", flowId, providerId, authType }
{ id, type: "auth_continue", flowId, requestId, value }
{ id, type: "auth_cancel", flowId }
{ id, type: "auth_status", providerId, refresh: false }

// Asynchronous, flow-correlated events; never emitted into session messages.
{ type: "auth_event", flowId, event: AuthEvent }
{ type: "auth_prompt", flowId, requestId, prompt: AuthPromptWire }
{ type: "auth_finished", flowId,
  outcome: "ready" | "cancelled" | "failed", status: SanitizedAuthStatus }
```

`AuthPromptWire` strips `AbortSignal` and preserves prompt kind and bounded
select choices. It must not expose arbitrary object serialization. Native
credentials remain native; status reports provider/method/readiness/source/
expiry classification, **not** key/token values. `auth_status` must be
side-effect-free by default, with any future refresh/remote verification an
explicit separate capability and user-authorized operation. Do not forward
arbitrary client `LoginOptions`; allow only declared safe options after review.

### RPC-side lifecycle and bounds

1. Validate provider, method, flow ID, sizes and concurrent-flow budget before
   invoking `login`. Retain exact begin identity in live memory so identical
   retries join the same live flow; conflicting reuse fails. Do not start a
   second provider login behind the runtime's serialization queue.
2. Allocate one flow `AbortController`; pass its signal into `AuthInteraction`.
   `notify` writes a bounded auth event, not a conversation event.
3. `prompt` allocates a unique request ID and one bounded pending promise. Keep
   per-prompt abort handling distinct from the flow signal. `auth_continue`
   checks flow/request identity and expected kind/choice IDs, consumes once,
   then resolves that promise. It must never also dispatch a session prompt.
4. Bound flow count, prompt count, text/choice sizes, frame size, flow TTL and
   output queues. The main stdin reader must continue while login awaits user
   input. Do not block unrelated observe/cancel commands on a pending prompt.
   Secret input needs short lifetime and dedicated memory, not ordinary debug
   request logging. Sanitize provider exceptions and strip raw diagnostics.
5. Only after runtime synchronization resolves emit `auth_finished: ready`.
   Readiness is not proof that a remote paid inference will succeed. Distinguish
   expired, missing, policy-blocked, network/rate-limited and provider failures.
6. Cancellation/disconnect abort pending promises and release listeners/timers;
   an ignored late callback cannot revive a terminal flow. Cancellation must
   not claim previously persisted credentials were rolled back. Return a safe
   cancelled/uncertain classification if provider effects may already exist.
7. Process restart discards flows and callbacks. Never persist/replay callback
   URLs, codes, PKCE material, keys, tokens or pending secret answers. Fresh
   status inspection can find native readiness; otherwise require a new login.
   An in-process live client reconnect can rejoin only under the same authenticated
   owner and flow generation. StdIO process loss cannot offer that guarantee.

Browser URLs, device codes and even seemingly informational messages can contain
sensitive material. Route their presentation only to the intended live client;
exclude them from history, model context, telemetry and trace payloads. Redaction
must apply at the plugin/host boundary as well as rendering. Never log full RPC
stdin/stdout for this feature.

### Bcode host integration

The plugin maps only the negotiated extension into portable harness auth
operations. Host authority remains binding-specific: plugin ID, instance ID,
credential namespace, session generation and current daemon owner must match.
A client cannot select another native account through arbitrary prompt text.

Host single-flight coordination may attach multiple managed sessions waiting on
one credential binding. Individual waiter cancellation detaches that waiter;
it does not automatically abort another session's shared login. Dedicated
sensitive-input transport supplies `auth_continue`; ordinary session `respond`
and non-secret Prompt must reject credential material. Do not persist the
native auth event just because it resembles an ordinary question.

After daemon restart inspect readiness in the same executable/agent directory;
do not reconstruct a flow from canonical history, use ambient account defaults,
or silently fall back from subscription to API-key billing. Host-vault custody
would require an explicitly supported ephemeral credential-delivery mechanism;
this proposal does not make native Pi persistence ephemeral.

## Required upstream and host tests before enabling auth RPC

* Callback mapping for all four prompt and all four notification kinds; select
  returns option ID; manual code and secret never reach ordinary session input.
* Begin/continue correlation, duplicate identical begin joining, conflicting
  IDs, stale response, out-of-order callbacks, late completion after cancel.
* Per-prompt and whole-flow aborts, timeout, disconnected writer, bounded output
  and flood limits; unrelated observe/interrupt still responsive.
* Runtime serialization/synchronization exercised, no direct credential-file
  writes; completion does not leak `Credential` on either success or failure.
* No credentials or callback artifacts in stdout diagnostics, stderr, traces,
  canonical events, model history, retained errors or snapshots. Device codes
  and URLs are treated as sensitive, not merely API keys.
* Native restart after credentials may have changed: reinspection, no callback
  replay. Shared waiters with one cancel, expired credentials, policy/network
  errors, no silent credential-source or billing-route fallback.
* Existing RPC clients continue to work when capability absent. Unsupported
  login fails before emitting a prompt or mutating credential state.

The offline fixture suite under `packages/bcode/tests/fixtures/harness` includes
native protocol/failure inputs and an acceptance matrix. It intentionally has
no invented Pi `auth_begin` success transcript. Concrete lane-11 CLI request
files and isolated fake-native application tests are now written; they remain
unexecuted. Startup integration, native capability verification, full native-auth
E2E and mixed workflow acceptance still need validation. No passing build,
runtime validation, or upstream compatibility claim is made here.
