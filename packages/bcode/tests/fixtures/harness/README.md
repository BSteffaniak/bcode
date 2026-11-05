# Offline managed-session acceptance fixtures

## Status and boundaries

These are test sources, not passing-test or native-compatibility evidence. No
build, formatting, test, native agent, login, provider request or architecture
check was run during the implementation wave.

`harness_cli_acceptance.rs` now uses **concrete lane-11 CLI and IPC contracts**:
`harness start/observe/send/respond/cancel/reconcile --request FILE` and
`harness history SESSION --after CURSOR --limit N`. It writes the actual
`ManagedSessionCall` JSON (`session_id`, association hints, and `harness` typed
request envelope). Responses use the actual `HarnessServiceResponse.kind`
`{kind,response}` shape; history returns a `HarnessObservationPage` directly.
There are no configurable argv bindings, expected-output adapters, JSON-pointer
overrides, test reducer, ignored tests, or external configuration inputs.

The application tests require `app` and
`static-bundled-claude-code-plugin` features, Unix executable permissions and
a Python 3 executable resolved from the controlled test PATH or explicit absolute
`BCODE_HARNESS_FIXTURE_PYTHON`. Resolution verifies existence/executable mode and
canonicalizes before subprocess environment clearing; missing Python fails rather
than skips. The coordinator owns plugin bundling and all real application
wiring. Missing canonical admission is a test failure, not a skip or a fallback
to direct plugin calls. Build/run only after the coordinator lifts the initial
validation restriction.

## Isolated real application setup

Each test creates a private temporary directory containing:

* A copy of `claude_native.py` with executable permissions. Its shebang uses
  the absolute resolved Python path in a rewritten shebang; it never runs Claude,
  a model provider or any network API. No `/usr/bin/python3` assumption is made.
* A selected native namespace and a fixed scenario file.
* Online configuration rendered from `claude-config.toml.in` with absolute paths
  and an exact fixture version. Only `bcode.claude-code` is enabled.
* Offline configuration with all plugins disabled; it uses the **same state
  root** after the test daemon is stopped, so ordinary artifact matching cannot
  accidentally leave the online plugin loaded during the offline history test.
* The explicit Cargo-built Bcode artifact's SHA-256 and a distinct `state` root.

The helper invokes `CARGO_BIN_EXE_bcode` explicitly, clears ambient environment,
sets `HOME`, XDG directories, an explicit `BCODE_CONFIG` for every invocation,
and `--state-root` under the temporary root. It records artifact SHA-256. Normal
client startup enforces matching client/daemon artifact identity. Cleanup uses
bounded `server stop --force --yes` against **only that test artifact/state**;
it never targets the active runtime, uses `stop-all`, or replaces a daemon.
Output, request, native frame and command deadlines are bounded. Cleanup does
not panic while unwinding an acceptance assertion.

The Claude double implements the currently used `--version`, native `auth
status` readiness inspection, retained `--session-id=UUID` / `--resume=UUID`,
initialize negotiation without a native session identity, user input followed by
`system/init` identity confirmation, tool question callback/response, interrupt
and explicit native state frames. A start must return `Prepared`, not `Opened`,
within the post-initialize deadline, before any user effect exists. A pre-turn identity wait must
fail this fixture rather than obtain invented confirmation. Empty/dummy prompts
are rejected, with a visible oracle marker. This is a fake
native executable, **not** a fake Bcode application. It asserts no real native
version supports these features. Version/capability booleans in the config refer
only to the fixture executable. Native callback material is not logged.

The timing assertion separates cold CLI/daemon startup from protocol latency.
Before first native launch it measures real `artifact-id`, `server start`, and
`server status` control commands. Every CLI invocation retains its 35-second
outer deadline. For managed start, the parent polls a fixed native
`initialize-replied.monotonic_ns` marker and enforces a separate five-second
initialize-to-Prepared return deadline, including the exit iteration. Native
stream-start/initialize-received/replied/first-user monotonic timestamps and CLI
elapsed durations are printed without raw frames or credential material. Thus
cold executable/config/daemon startup cannot consume a fabricated ten-second
protocol budget, while an old pre-turn identity wait still fails boundedly.

Fixed `*.effect` labels are test oracles, never application recovery records.
Exclusive creation records `*.duplicate` and fails on a repeated effect even
when a retry changes request ID. No input body or secret enters those files.

## Written concrete CLI checks

* Retained managed open returns a stable **Prepared** target with no confirmed
  native ID. Requests use lane 1's exact
  `{ "state": "prepared"|"confirmed", "session": ... }` target encoding;
  Submit/Observe/Respond/Interrupt route through `target`, never a sentinel binding.
  First real submission triggers a `binding_confirmed` observation checked against
  open/session/instance/generation/version/native-selection correlations, then
  response/cancel use the Confirmed target. Identical submission
  returns recorded evidence, changed payload fails.
* Native AskUserQuestion becomes a canonical typed interaction. Stale generation
  fails before a native response; current answer and identical retry deliver once.
* Cancellation exposes acknowledgement, not invented settlement; one actual
  native interrupt effect is observed.
* Populated canonical history uses two-item pages without repeated observation
  IDs; plugin-disabled restart retains the committed prefix.
* Ordinary managed child creation is discoverable through actual
  `session children` output, rather than invented child metadata in native frames.
* Native delivery followed by immediate process exit and Bcode daemon restart:
  reconciliation never infers not-applied; identical send returns the canonical
  retained receipt; no duplicate effect marker appears.
* Loss after first native input but **before** native confirmation stays uncertain;
  no BindingConfirmed history, replay, or new stream launch is allowed. A fresh
  submission ID cannot authorize recreation of the dead Prepared transport.
  Prepared interruption is tested without submitting or confirming any native turn.
* Unsafe native result diagnostic sentinel must be absent from CLI output and
  offline canonical history; malformed secret-bearing auth input is rejected
  without echo. Oversized request files fail before dispatch.

`harness_security_contracts.rs` additionally exercises actual public
`ManagedAuthCoordinator` and IPC decode APIs: shared waiters issue one begin,
one cancellation does not abort the remaining waiter, generation fencing,
sensitive debug redaction, one-shot continuation, stale callback rejection,
restart epoch rejection and forged/unbounded history requests. These are
**public contract tests, not native-auth/daemon E2E**. No duplicate auth types or
fake successful Pi login RPC are defined.

## Other native protocol fault inputs

`harness_native_fixtures.rs` exercises the independent JSONL fault injector
`fake_native.py --scenario NAME --oracle ABSOLUTE_TEST_DIRECTORY`. The Python
path resolves from the controlled test PATH and can be explicitly overridden by
`BCODE_HARNESS_FIXTURE_PYTHON`. Only explicitly captured request correlations are
substituted into scripted frames. Gates use `NAME.waiting` / `NAME.release`
barriers, never silence-as-success. The oracle may also be selected through
`BCODE_HARNESS_TEST_ORACLE`; production plugins need not forward that variable.

`scenarios.json` covers Pi handled-without-turn, foreground end before
`agent_settled`, native questions and non-security UI confirm, stale reply before
current reply, lost prompt/abort acknowledgement, unsafe diagnostics, Codex
lost `turn/start` acknowledgement/expected-turn mismatch, Claude foreground
result before background continuation, malformed and unterminated frames.
The Claude marker field is `state`, matching the current Rust protocol library.
These tests prove fault-input behavior only; do not count them as host acceptance.
Two additional native tests prove empty initialize output, absence of pre-user
identity, first-user-only system/init, and rejection of dummy prompts. The
application suite now contains nine tests when `daemon-client` and Unix are available;
new terminal coverage remains unexecuted by this lane.

## Dedicated client-terminal handoff acceptance

`client_terminal_handoff_is_offline_one_shot_and_history_clean` requires Unix,
`app`, `daemon-client`, and `static-bundled-claude-code-plugin`. It uses the real
`harness auth handoff --request <file> --execute` CLI, not daemon stdio or an
invented auth-login command. The request selects the enabled plugin's declared
`bcode.claude-code.handoff/v1` extension. That generic extension supplies actual
`native-login`/`client_terminal` capability evidence with native inactivity still
`unknown`; the fixture never asserts inactivity. The explicit offline executable
reports version `2.1.308` and simulates `auth login` only in `terminal_auth` mode.
The fixture starts locally ready so both real stream transports can initialize;
only after their genuine Prepared proofs does the test simulate auth expiry.
Setting login-required before open prevents native stream startup by design.
No real native executable, credential store, browser, network or provider is used.

`terminal_auth.py` supplies real PTYs, retains native secret-bearing output only
in bounded memory, and starts a competing dedicated terminal while the first
child is unreaped. Its independent phases allow 40 seconds for owner native
startup, 40 seconds for competitor startup/denial, and 30 seconds for owner
completion, each capped by a 120-second total deadline. Native login waits at
most 90 seconds for release; the outer Rust wrapper allows 125 seconds for
bounded diagnostics/cleanup. Ordinary single CLI operations retain 35 seconds
and initialize-to-Prepared proof retains its existing five-second bound. These
fixture-only allowances accommodate measured cold startup up to 28 seconds;
they do not change the production explicit-target verifier or classify errors
as timing failures. Exactly one fixed-label login effect is allowed; a duplicate
claim/execution must fail. Native output must reach only its owning terminal.
After exact child reap, the real CLI continues/reinspects readiness and returns
settled/ready. Fixed-label stage/condition diagnostics, elapsed milliseconds and
numeric child exit codes reach stderr. Both terminals request `--json-errors`;
on failure the driver emits only an allowlisted code from a schema-v1 typed CLI
diagnostic, never its message. Missing/ambiguous codes are reported as such;
managed-auth failures may currently normalize to `unclassified_application_error`.
Transcripts, exception messages, paths,
flow handles and decoded native snapshot values never do. These distinguish
owner exit before native launch, competing-claim failure, owner exit before
release, readiness/snapshot failure and deadline exhaustion without exposing
native output. Only a fixed secret-free summary reaches the outer test stdout.
Canonical history must be identical before and after the complete flow.

A separate isolated test process uses `DaemonTarget::verify` against the
Cargo-built executable and `BcodeClient::for_daemon`, preserving artifact/state
identity rather than bypassing the handshake. Candidate discovery is bounded to
64 records in the private fixture state. The selected daemon executable must
hash to the exact Cargo-built fixture bytes; its canonical path is then passed
to normal executable-identity probing and live handshake verification. Registry
artifact metadata alone never establishes authority, and no alternate executable
or identity is accepted if verification fails. This avoids reprobe of the mutable
Cargo output path when the daemon is running its verified retained image.
It explicitly creates its isolated
XDG configuration directory before target verification (an explicit config file
alone need not create that directory). A bounded 8 KiB sidecar records only fixed
setup/operation labels, response variants and allowlisted normalized error codes;
parent output includes that safe sidecar and numeric child exit, never child
stdout/stderr, panic payloads, plans or handles. On `IdentityUnavailable`, the
public verifier has already erased the underlying source reason. A diagnostic-only
12-second `artifact-id` invocation of the same hash-verified canonical executable
records fixed labels for timeout/I/O/exit/encoding/shape failures and a boolean
comparison against the advertised artifact, never raw output. Its result cannot
construct a target, retry admission or replace the original verification failure.
The probe watchdog allows this extra diagnostic time; the canonical verifier's
five-second identity deadline and all claim/replay assertions remain unchanged.
Coordinator diagnostics have observed `exceeded_verifier_5s` together with
`valid_matches_advertisement`: the exact executable returns a valid matching
identity, but too late for normal verification. This is an explicit-target startup
deadline blocker, not missing artifact metadata. `DaemonTarget` has no public
retained-target constructor that can avoid the required executable probe. The
fixture must not synthesize one, accept the diagnostic result as authority, use
a shim executable, or retry until a convenient timing passes. Resolution belongs
to the lifecycle owner: optimize the real identity startup path or provide an
explicit bounded identity-probe timeout selection while retaining hash, identity,
namespace, storage/protocol and live-handshake checks. Until that lands and the
whole test passes, the same-socket acceptance remains blocked.
Two real IPC connections share one
namespace reservation before claim. After daemon restart, old sessions are used
only for readonly status/history checks; distinct new sessions must complete real
Prepared initialization on the current daemon before the one-plan probe. Fixture
auth is locally ready for those fresh opens, then expired again for preparation.
No old native transport is implicitly reacquired or replayed. Stale interaction IDs, repeated plan claims,
competing waiter claims, continuation before reap, and replay from a replacement
connection are rejected. This probe claims but deliberately does not execute a
second plan: it must not fabricate a reap to clear ambiguous delivery. The
isolated daemon/state is cleaned up afterward. The first successfully reaped
flow's fence is independently shown released by restart and fresh preparation.
New terminal acceptance has not been run in this lane; coordinator validation
is required.

## Daemon auth acceptance and fresh-begin boundary

`daemon_auth_status_is_sanitized_and_missing_native_admission_blocks_shared_begin`
uses the real CLI/daemon route for two canonical sessions sharing one native
namespace. The existing supported native status fixture emits ready then
login-required, with a deliberately secret-bearing ignored diagnostic; public
status must expose sanitized readiness/custody/namespace only. The test checks
begin fails closed for both would-be waiters, cancel without a live handle fails,
ordinary stdout cannot request sensitive interactions, history is unchanged, and
restart re-inspects readiness. A fixed marker detects any unintended native
`auth login` launch. This is not a successful shared-login lifecycle test.

Exact blocker: Claude `AuthFlows::invoke(AuthStatus)` constructs only
`ManagedAuthStatus`; it does not emit the optional admission extension containing
plugin-selected `method_id` and positive `native_login=inactive`. Canonical
`authorize_auth_inspected` therefore leaves `fresh_begin_authorized=false`, and
`begin_auth` rejects before dispatch. Readiness/login-required is not inactivity
proof. The fixture must not invent that extension or bypass fresh-begin admission.
This boundary does not prohibit the distinct declared client-terminal preparation
path covered above: terminal execution has its own scoped reservation, one-plan
claim and durable unreaped-child fence, without claiming native inactivity.
Structured sensitive callbacks and daemon-owned fresh-login execution remain
unsupported by this fixture. No test/build was run here.

## Remaining gaps

* Structured sensitive callbacks, expiry during a native terminal interaction,
  terminal cancellation and real-account verification remain outside the new
  offline terminal acceptance. Shared reservation, one-plan claims, dedicated
  output, reap/reinspection and stale replay now have concrete coverage written.
* Mixed ordinary/external durable workflow nodes, parent assistance during a
  native background wait, owner transfer races and workflow terminalization
  require real workflow authoring/admission and authority fixtures. No fictional
  workflow success command or fabricated native parent scope is substituted.
* Real native capability verification remains deferred. Extension UI confirm is
  not a comprehensive Pi security-approval boundary. Pi has no current login RPC.
* Stale native correlations/background continuation are deterministic protocol
  fixtures, not yet real-daemon application E2E scenarios.
* Native tools remaining inert, canonical trace/telemetry secret scanning and
  damaged-history completeness/repair behavior need additional application
  instrumentation; CLI/history assertions cover only their observable boundary.
* New terminal acceptance remains uncompiled and unexecuted by this lane;
  prior daemon-readonly acceptance validation is coordinator-owned.
