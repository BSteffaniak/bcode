# Codex managed harness plugin

Native `codex app-server --listen stdio://` backend for shared `bcode.harness/v1`,
not a model provider. Protocol source is explicitly pinned to Codex
`8553558345de65dddf41ca4f6200e5b7b84d9cf9`; the sibling checkout HEAD is different.

## Integration and API

* Plugin ID `bcode.codex-harness`; auth provider `bcode.codex`, vault method and
  credential `api_key`, storage key `CODEX_API_KEY`. No ambient credential discovery.
* `static_plugin()` under `static-bundled` returns the SDK static vtable and excludes
  dynamic exports. Distribution enablement is host-owned (`StaticBundledPlugin::new`
  defaults enabled); no manifest `enabled_by_default` field exists.
* Shared requests consume only `HarnessSessionTarget::Confirmed`. Prepared targets
  fail closed. Thread start returns a genuinely native-confirmed `Opened` binding;
  no fabricated Prepared state or BindingConfirmed observation is needed.
* Explicit absolute executable, cwd and `CODEX_HOME` are required. Config namespace
  must equal credential namespace. Initialize must report the admitted CODEX_HOME.
  Inspect/auth requires host config `auth_working_directory`.
* Public implementation API: `CodexSession::{open, open_with_key, open_admitted,
  binding, submit, observe, status, respond, interrupt, interrupt_active, reconcile}`;
  `CodexAuth::{open, status, begin, poll, cancel, vault_key}`; `Launch`, `CodexError`.
* Dependencies are existing workspace `bcode_harness_models`,
  `bcode_provider_auth_models`, `bcode_plugin_sdk`, `serde`, `serde_json`, `thiserror`,
  `zeroize`, `uuid` (`v4`), and test-only `tempfile`. No new external version.

## Placement and lifecycle limits

**A fresh, non-resumed local-only thread supports exactly one first `NextTurn`.**
Before creation the adapter calls `remoteControl/disable` with `ephemeral:true`,
verifies `remoteControl/status/read` reports `disabled`, and arms continuous reader
fencing against any later activation (including notifications already queued).
Native delegation is disabled/verified and `daybreakEnabled:false` is explicit.
The newly returned thread must be idle with empty turns. Private stdio and the
new process-local thread—not a local idle snapshot—provide the construction proof.
Any observed unexpected native start revokes it. The single first-input permit is
consumed before one `turn/start`; an identical immediate retry returns its retained
receipt and ambiguity permanently fences further mutations.

**Later NextTurn and all resumed-thread NextTurn remain unsupported.** Pinned
`turn/start` invokes `start_or_steer_turn` (`request_processors/turn_processor.rs:661`)
without an atomic idle precondition. Core emits completion before clearing active
state (`core/src/tasks/mod.rs:860–875`), so even observed completion cannot safely
release another start. Genuine conditional app-server start is still needed;
Core's `start_turn_if_idle` is not exposed by this pinned RPC. Expected-turn steering
continues to use `turn/steer`. No upstream or dependency changes were made.

Top-level `--version` (not `app-server --version`) is probed with bounded output,
deadline and cleanup. Initialize userAgent is not used as executable version identity.
Resume requires the same verified version and exact native binding; native history
is excluded. Read is metadata-only. No unrelated-session adoption or ambiguous replay.

Pinned notifications are interpreted as actually defined:

* `thread/started` carries `{thread}`, not `{threadId}`.
* `turn/started` and `turn/completed` carry `{threadId, turn}` with status/error.
* Started/completed produce normalized status and turn correlation. Failure produces
  a sanitized `native_turn_failed` diagnostic, not native error text. Interrupted
  and failed turns never produce successful settlement. Delayed duplicate completion
  cannot reopen a terminal turn.
* Successful exact active-turn completion may produce native `Settlement`, never
  assignment acceptance. Native multi-agent operation is disabled and effective
  `features.multi_agent=false` verified before opening. Observed collab activity,
  pending controls, error evidence or any observation gap prevents settlement.
  Background continuation remains unsupported. One-entry deferred settlement makes
  this evidence independent of caller page size.

Host owns canonical retained mutation identities, durable deduplication, permission,
parent-route reauthorization and generation fencing. Exact `turn/start` response
`turn.id` or `turn/steer` response `turnId` now produces one retained
`SubmissionAccepted { submission_id, attempt_id, native_turn_id }` observation,
with enclosing owner/session/generation. It is emitted before lifecycle evidence;
an identical submit retry does not enqueue another mapping. One pending mapping
backpressures subsequent submissions until observed. Rejected, uncertain, malformed
or cross-shaped acknowledgments never manufacture this association. Receipt
`native_request_id` is the actual JSON-RPC request identity, not the turn identity.
The plugin caches only last
submission/page/control replies, not a recovery journal. Lost native open acknowledgment
is tombstoned for this live owner; restart reconciliation never guesses no effects.

## Bounded transport and control

Independent stdout/stderr readers drain continuously. Frames, event bytes, owners,
callbacks and page caches are bounded. Queue overflow fences the entire transport,
wakes pending RPCs as uncertain, withdraws queued writes and asks the supervisor to
kill/disconnect the process. It never silently drops a callback while leaving the
native process alive waiting for its response.

Every queued frame has a deadline; RPC timeout fences the entire connection. Writer
checks deadline/fence before delivery; a blocked write is monitored and terminated
by process disconnect. Bytes already in flight remain explicitly uncertain (cannot
be retracted). Later mutation replay is forbidden. Child ownership/reaping is in a
supervisor thread; invocation/drop does not synchronously wait on process exit.
Probe cleanup is likewise bounded for the caller. Native loss is never settlement.
Host cancellation during a synchronous RPC remains bounded by that RPC's deadline;
immediate per-invocation cancellation plumbing is not yet integrated.

Approvals/questions use exact native request IDs and generation. File approval needs
a retained bounded diff and no root grant; absent/evicted diff is declined. Additional
permissions/network grants are declined. Secret generic questions are not exposed.
Multi-question answering remains unsupported. Unknown callbacks are rejected; wrong
thread callbacks cannot acquire authority. Arbitrary dynamic tools are refused,
never dispatched into Bcode. `bcode_parent_assistance` is registered/interpreted only
with an explicit validated host assistance grant; host must reauthorize responses.

## Authentication

Browser/device login uses supported public native RPC. URL/code are returned only
through sensitive auth effects. Host must suppress sensitive request/response tracing,
history and telemetry. Native login owns storage/refresh; no empty vault enrollment,
credential scraping, or internal token-login route exists.

Scoped vault-key delivery requires effective `cli_auth_credentials_store=ephemeral`
and explicit uncustomized OpenAI provider selection before any key is sent. Each
native session receives its own scoped key: upstream ephemeral storage is process-local.
Encoded transport buffers are wiped and stderr/error payloads are not retained.

Correlated login completion is retained before fallible readiness reinspection;
failed reinspection returns terminal flow evidence with unknown readiness instead
of waiting forever for a consumed notification. Last terminal reply is retryable.
Cancel sends once: native `canceled` becomes login-required; `notFound` becomes
blocked/uncertain, not proof of cancellation. Lost cancel acknowledgment is fenced.
Local readiness never means remote verification. One live flow per auth process;
namespace-sharing/waiter cancellation belongs to the host. Flow data is ephemeral
and never replayed after daemon restart.

## Source-only tests / remaining acceptance

Offline fake-process fixture models pinned started/completed/failed/interrupted
shapes, top-level version args, callbacks, config, auth completion and cancellation.
Twenty integration tests and transport/target unit tests cover provenance, resume,
steering, NextTurn refusal, uncertain delivery, blocked writes/bounded teardown,
overflow disconnect, status/settlement, stale callbacks, tool isolation, vault gates,
cancellation result distinction and completion surviving status failure.

first-start construction, one-shot/retry semantics, lost first acknowledgment,
remote-control refusal/activation fencing, and resumed/later-start refusal.

**Not executed:** fmt, compile, tests, clippy, architecture checks, plugin builds,
native version/app-server/login or provider requests. This is source implementation,
not acceptance. Real installed-version/managed-policy compatibility, host integration,
later conditional-start execution, remote verification and immediate cancellation remain gaps.
