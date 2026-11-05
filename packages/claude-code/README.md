# bcode_claude_code

Independent Rust-native Claude Code CLI library. No Python/Node bridge, Bcode
session/workflow dependency, credential-store scraping, recovery journal, native
installation, or automatic mutation retry.

## Integration API

* `LaunchOptions`: explicit absolute executable/cwd/config-directory, exact trimmed
  `--version`, complete environment (child uses `env_clear`), billing route,
  settings inheritance, exact resume identity, capability attestations and bounds.
* `ClaudeSession::spawn` verifies version and native local auth status before
  starting continuous stdout/stderr drainers. `spawn_with_sdk_servers` additionally
  registers bounded SDK MCP names and delivers `McpMessage` callbacks to the owner.
  No native-observed tool is dispatched by this library.
* `ClaudeSession::spawn_new(options, uuid)` creates with native `--session-id`;
  UUID syntax is validated and simultaneous resume is rejected before process creation.
  `spawn_new_with_sdk_servers` combines this with SDK MCP registration.
* `initialize()` negotiates control readiness only; it does not wait for identity.
  `is_control_ready()` distinguishes this from identity evidence. `identity_state()`
  returns `Unselected`, `Prepared { requested_session_id }`,
  `Confirmed { native_session_id }`, or `Uncertain` after a fault/disconnect.
  The host must represent prepared open separately from native-confirmed binding.
* Submit the real authorized first task with the selected ID (or omit the frame ID
  and use the process launch selection). A later `system/init` confirms identity;
  byte-for-byte create/resume mismatch faults the process before exposing that init.
  Requested IDs are never returned as native confirmation. No dummy prompt is sent.
  `wait_session_identity()` observes confirmation without consuming the event queue;
  its bounded timeout faults the process. `initialize_session()` remains a strict
  initialize-and-wait convenience API, **not** the ordinary prepared-open path.
  Control initialization in the statically inspected 2.1.308 artifact lacks a session
  ID; its ordinary system init is constructed in the submission path. An explicit
  future initialize-response ID can also confirm identity, but is not assumed.
* `client()` returns a cloneable process-scoped `ClaudeClient`. Call
  `initialize()` once before `submit`, `request`, or `interrupt`. Initialization
  with hooks uses `request(ControlRequest::Initialize { ... }).await?.wait()`.
* `request` returns an independently awaitable `ControlTicket`. `cancel`/drop
  abandons the local waiter, **not** native execution. Incoming native
  `control_cancel_request` invalidates callbacks and suppresses queued responses
  not yet admitted by the writer. Cancellation racing an already-started write
  cannot guarantee no effects. `respond` correlates callback identity and kind.
* `submit` acknowledges only bytes written. Active-turn steering is rejected;
  subsequent submission requires authoritative native settlement. Unknown-mode
  sessions permit a first turn but cannot claim settlement or accept another turn.
* `next_event` delivers bounded ephemeral native events. Callback handling never
  blocks the reader. A slow consumer, oversized JSONL frame, unsupported protocol,
  or exhausted request/task bound explicitly faults/kills the transport rather
  than silently dropping an observation and continuing with incomplete authority.
* `settlement()` is a current observation, not durable Bcode assignment completion.
  Result alone, idle alone, task completion, exit(0), interruption ack, and elapsed
  quiet time are never settlement. Verified mode requires a current-activity result
  and idle in either order, no active tracked tasks and no unanswered callback.
  Submission/new main-thread assistant activity invalidates prior result/idle.
  Task lifecycle changes invalidate prior idle, but task completion does not invent
  a mandatory extra result: a fresh authoritative idle and the current run result
  can settle. Unknown task classes disable
  authoritative settlement. Native process loss is uncertain.
* `interrupt` is cooperative control only. `terminate` and `close` kill/reap the
  owned process and invalidate callbacks; these never imply rollback. The host
  still owns durable cancellation/reconciliation and generation fencing.
* `auth_status` returns sanitized **local** readiness and route, not remote
  verification. Requires an exact-version attestation for status/login including
  `configDirectory` (official docs identify v2.1.268+). Unknown support fails closed.
* `login_handoff` is explicitly authorized native `auth login` with inherited
  terminal stdio in the same namespace. It returns `NativeLoginHandoff` with
  `wait`/`cancel`; it does not invent JSON auth continuation or persist URL/code
  material. The caller must provide a dedicated trusted terminal, coordinate
  actual credential namespaces/shared waiters, and reinspect status afterwards.

Native raw `Value` bodies may contain secrets and untrusted content. They must not
be traced, logged, persisted, or forwarded into model context directly. The plugin
must normalize/redact them, correlate them to its generation, and fence ownership.
`NativeEnvironment`/`LaunchOptions` intentionally do not implement `Debug`.

`CLAUDE_CONFIG_DIR` scopes native configuration but must not be treated as a proven
universal credential-isolation guarantee: current authentication documentation
also describes account/profile modes with storage outside that directory. Only
explicitly verified native-account/API-key routes are admitted; profile, token,
custom endpoint and third-party routes fail closed. Managed policy is not bypassed.
Environment credentials and settings-helper precedence are checked by native
status before a stream launch. External changes after that check remain a native
namespace ownership concern for the embedding host.

## Protocol references

Protocol source: Anthropic public Python SDK commit
`2d485201617258867dd0f74b9e7307056f5938fe`, especially
`_internal/query.py`, `_internal/transport/subprocess_cli.py`, and `types.py`.
It is a reference, **not a dependency**. Official CLI-reference/authentication docs
were consulted for auth commands/namespace/billing behavior. Session-state and
native-auth support require operator-tested exact-version attestation, not a
version-number guess. Hooks and SDK MCP callbacks are supported transport shapes;
the library does not supply a generic MCP tool implementation.

## Validation status / coordinator wiring

Tests are written in `tests/fake_process.rs` using local shell fixtures, never an
installed Claude executable. No compilation, formatting, tests, architecture
checks or live CLI invocation were run during the initial writer wave.

Coordinator must register `packages/claude-code` in workspace members and add
`bcode_claude_code = { path = "packages/claude-code" }` to workspace dependencies.
Dependencies use existing workspace entries: serde, serde_json, thiserror, tokio;
`tempfile` is test-only. Validate formatting, clippy, compilation and fake-process
behavior after the wave. Production exact-version compatibility, managed-policy
behavior, platform-specific login UI, real SDK MCP interoperability and native
background settlement remain unverified without authorized isolated testing.
