# Codex managed harness plugin

Native `codex app-server --listen stdio://` backend for the shared `bcode.harness/v3`
contract, not a model provider. Verified against the installed Codex CLI `0.164.0`
(app-server experimental API surface).

## Integration and API

* Plugin ID `bcode.codex-harness`; auth provider `bcode.codex`, vault method and
  credential `api_key`, storage key `CODEX_API_KEY`. No ambient credential discovery.
* `static_plugin()` under `static-bundled` returns the SDK static (concurrent) vtable and
  excludes dynamic exports. Distribution enablement is host-owned.
* Services declared in `bcode-plugin.toml`:
  * `bcode.harness/v3` — managed sessions;
  * `bcode.harness.candidates/v1` — setup discovery (`discover_candidates`,
    `prepare_candidate`);
  * `bcode.harness.models/v1` — native `model/list` discovery and exact selected open;
  * `bcode.harness.recovery/v1` — exact resume preflight and physical release evidence;
  * `bcode.harness.terminal-auth/v1` — client-terminal `codex login` preparation.
* The plugin is `concurrent`: operations serialize on retained state, and only an
  `observe` with `wait_millis` waits for native output outside that lock (woken by the
  transport reader, never polled).
* Shared requests consume only `HarnessSessionTarget::Confirmed`. Thread start returns a
  genuinely native-confirmed `Opened` binding.
* Explicit absolute executable, cwd and `CODEX_HOME` are required. Config namespace must
  equal credential namespace. Initialize must report the admitted `CODEX_HOME`.

### Configuration

`plugins.config."bcode.codex-harness"` decodes into `config::Config`:

```toml
# Optional; defaults to the instance's CODEX_HOME.
auth_working_directory = "/absolute/dir"

[instances.codex]            # written by setup discovery
executable    = "/etc/profiles/per-user/me/bin/codex"   # PATH spelling, not the store path
codex_home    = "/home/me/.codex"
exact_version = "0.164.0"
```

A present instance entry must match the generic instance exactly (executable, namespace,
`bcode.codex` provider, native or vault custody); otherwise every operation fails closed.
Absent entries keep the legacy behavior of using the generic instance alone.

### Setup discovery

`discover_candidates` searches absolute `PATH` entries for `codex` (relative and duplicate
entries skipped, bounded to `MAX_HARNESS_CANDIDATES`), records the canonical file identity
and the PATH invocation spelling (`HarnessCandidateExecutableIdentity::invocation_path`),
probes the top-level `codex --version`, and inspects readiness in `$CODEX_HOME` (default
`~/.codex`) with `account/read {refreshToken:false}`: a signed-in account or a configuration
that needs no OpenAI login is `existing_native_login`; an absent account that requires login
is `login_required`. No thread, prompt or token refresh happens. An ambient `OPENAI_API_KEY`
or `CODEX_API_KEY` refuses discovery rather than selecting another billing route.
`prepare_candidate` re-runs discovery, requires exact equality with the selected candidate,
and returns native-custody instance settings that the adapter's own decoder accepts.

## Turn admission

* The first `NextTurn` of a fresh thread uses the single-use construction permit (private
  stdio, remote control disabled and fenced, delegation disabled, idle empty thread).
* **Later `NextTurn` is admitted once the previous accepted native turn's completion
  (`turn/completed`) has been observed, no callback is pending and no acceptance is
  undelivered.** A resumed thread admits its first `NextTurn` when `thread/resume` reports
  it idle. This is foreground admission only, exactly like the Claude Code adapter: it never
  satisfies a completion boundary, and retained background work keeps its own evidence.
  Pinned `turn/start` still has no atomic idle precondition, so a native start racing an
  unobserved automatic follow-up is reported as native evidence rather than prevented.
* Expected-turn steering continues to use `turn/steer`. An identical submit retry returns the
  retained receipt; ambiguity permanently fences further mutations.
* Exact `turn/start` `turn.id` / `turn/steer` `turnId` produces one retained
  `SubmissionAccepted` before lifecycle evidence; one pending mapping backpressures later
  submissions until observed.

## Transcript (native parity)

Normalization follows `docs/claude-code-native-parity-contract.md`:

* Agent messages become `Text { segment_id: Some(<native item id>) }`; a `final_answer`
  message in the accepted turn additionally yields an authoritative `Result`.
* Reasoning items become `Reasoning { segment_id: Some(<item id>) }` with summary parts joined
  by a blank line (raw content when no summary); encrypted reasoning without text is omitted.
* `commandExecution`, `fileChange`, `mcpToolCall`, `dynamicToolCall`, `webSearch` and
  `imageView` items become `Tool` (bounded `input_json`, oversized inputs summarized) on start
  and `ToolResult` (bounded output, tabs expanded, error from status/exit code) on completion.
  Observed native tools never dispatch Bcode tools.
* `thread/tokenUsage/updated` per-request counters are summed per turn and emitted as one
  `Usage` before turn completion (cache reads separated from input; model is the thread's
  current native model).
* `item/agentMessage/delta` and `item/reasoning/summaryTextDelta` become the transient `live`
  sidecar keyed by the same item id (bounded, coalesced; dropped rather than queued under
  pressure). The transport reader merges adjacent deltas and drops notifications the adapter
  does not interpret (output deltas, diffs, rate limits, warnings), so streaming cannot
  exhaust the bounded queue. A waiting observe holds a live-only page for 40 ms.
* Pages with nothing to transfer carry no retention page and no successor cursor.
* Approvals carry `tool_name` (`shell` / `apply_patch`) and a bounded `input_preview` (command,
  cwd and reason, or the retained diff).
* Failed turns produce a closed-vocabulary `Diagnostic` (`native_unauthorized`,
  `native_usage_limit`, `native_rate_limited`, `native_context_window`,
  `native_connection_failed`, `native_turn_failed`); native error text is never retained.
  Status, execution and acceptance evidence produce no transcript item.
* A successful turn whose last message has no phase (providers without message phases) uses
  that message as foreground output only when no `final_answer` arrived; commentary never is.

## Mid-session configuration

`Configure` uses native `thread/settings/update` (applies to subsequent turns):

* `Model { model_id }` passes the native model key unchanged (keys come from `model/list`);
  native rejection is `Failure(Policy)`.
* `PermissionMode::Default` restores `untrusted` approval and the opening sandbox;
  `AcceptEdits` selects `on-request` approval with a workspace-write sandbox; `Plan` selects
  native plan collaboration mode (requires a known thread model).

Identical retries return the retained outcome; an uncertain call fences the session and is
never replayed.

## Resume and recovery

`prepare_resume` validates an exact host recovery intent without spawning: same generation and
instance, the configured exact version, a well-formed native thread id and an absolute
workspace. The resumed open re-probes the executable (the version must match), disables and
fences remote control, and calls `thread/resume {threadId, excludeTurns:true}`; native history
is never replayed into Bcode and prior input is never resent. `release_transport` answers only
for the exact retained Close whose app-server the supervisor physically reaped.

## Bounded transport and control

Independent stdout/stderr readers drain continuously. Frames, event bytes, owners, callbacks
and page caches are bounded. Queue overflow fences the entire transport, wakes pending RPCs as
uncertain and asks the supervisor to kill the process; it never silently drops a callback.
Every queued frame has a deadline; RPC timeout fences the connection. Child ownership/reaping
is in a supervisor thread. Native loss is never settlement.

Approvals/questions use exact native request IDs and generation. File approval needs a
retained bounded diff and no root grant; absent/evicted diff is declined. Additional
permissions/network grants are declined. Secret questions are not exposed. Multi-question
batches are exposed sequentially. Unknown callbacks are rejected; wrong-thread callbacks cannot
acquire authority. Arbitrary dynamic tools are refused. `bcode_parent_assistance` is
registered only with an explicit validated host assistance grant.

## Authentication

Browser/device login uses supported public native RPC; URL/code are returned only through
sensitive auth effects. Readiness is `Ready` for a signed-in account or a configuration that
needs no OpenAI login. Scoped vault-key delivery requires effective
`cli_auth_credentials_store=ephemeral` and explicit uncustomized OpenAI provider selection.
Local readiness never means remote verification.

The plugin declares `bcode.harness.terminal-auth/v1`: `prepare_login` returns a client-terminal
plan running `<executable> login` with `CODEX_HOME` set to the instance namespace and only
non-secret environment (`HOME`, `USER`, `PATH`, `TMPDIR`, `LANG`, `LC_ALL`, `TERM`). The
matching `native-login` auth method only reinspects readiness on begin/continue (a fresh
inspection app-server each time) and cancels the local flow only. Nothing is launched in the
daemon. Note that native `codex login` replaces the namespace's existing `auth.json` as soon as
it starts, even if the browser flow is later abandoned.

## Unsupported

* Background continuation and authoritative settlement after native background processes:
  native turn completion does not close background terminals, so no `Settlement` is
  fabricated.
* Native multi-agent delegation (disabled and verified before opening).
* Atomic conditional start: app-server `turn/start` has no idle precondition.
* Resume across a native version change (the binding records the probed version).

## Tests

* `cargo test -p bcode_codex_harness_plugin` — unit tests and an offline fake app-server
  (`tests/fixtures/app_server.py`) covering admission, steering, retries, callbacks,
  lifecycle, live/segmented transcript, overflow and auth.
* `tests/native_subscription.rs` (`#[ignore]`) — one real first turn through the session API.
* `packages/bcode/tests/harness_codex_acceptance.rs` (`#[ignore]`) — daemon journey: selected
  model, approval, later turn, model switch, restart, recovery and resumed context:

  ```sh
  cargo test -p bcode --features app,static-bundled-codex-harness-plugin \
    --test harness_codex_acceptance -- --ignored --nocapture
  ```

  `BCODE_NATIVE_CODEX_HOME` selects the native namespace (default `$CODEX_HOME`, `~/.codex`).
