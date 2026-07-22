# Deterministic SDK provider tests

Enable Bcode's opt-in `testing` feature to use a public, network-free provider fixture:

```toml
[dev-dependencies]
bcode = { version = "0.0.1-alpha.0", default-features = false, features = ["testing"] }
```

`bcode::testing::ScriptedProvider` implements the production `ModelProviderInvoker` boundary. It
consumes a finite sequence of `ScriptedProviderTurn` values and captures the complete request seen
at every provider start. No native plugins, daemon, TUI, credentials, or network are required.

## Scripts

A turn is an ordered list of poll actions:

* `events(...)` returns the exact supplied `ProviderTurnEvent` batch. It intentionally performs no
  normalization, so tests can emit complete responses, partial streams, usage, warnings, provider
  metadata, tool calls, malformed lifecycle sequences, normalized provider errors, or unsupported
  event combinations.
* `delay(...)` waits with Tokio's clock and then returns an empty batch. Applications that enable
  Tokio's paused-time test support can advance this clock without wall-clock waits.
* `poll_error(...)` fails the provider poll operation.
* `pending()` remains pending until the SDK cancels or times out the operation.
* `start_error(...)` fails before a provider turn starts.
* `cancel_error(...)` and `finish_error(...)` exercise lifecycle-cleanup failures.

`complete_text(...)` and `provider_error(...)` are shortcuts for common canonical event sequences.
An exhausted nonterminal turn remains pending instead of busy-spinning. Starting more turns than
were configured returns a typed, non-retryable `script_exhausted` provider error.

Because scripts are ordinary finite Rust values, retry and fallback tests put a failing turn before
a successful turn. Tool continuation tests put a tool-call turn before the continuation response.
The runtime—not the fixture—continues to own retry, fallback, timeout, cancellation, validation,
tool execution, middleware, and session orchestration.

## Request and lifecycle assertions

A provider's `probe()` shares capture state with all its clones. It snapshots:

* complete post-middleware `ModelTurnRequest` values in start/continuation order;
* the provider plugin selected for each attempt;
* provider cancellation calls; and
* provider finish calls.

`ScriptedRequestExpectation` performs exact checks for configured provider/model selection,
messages, tool definitions, structured-output request, model parameters, and metadata while
ignoring fields the test does not configure. `assert_requests` reports the zero-based request and
exact mismatched field. The raw request snapshots remain available for custom assertions.

See the executable
[`scripted_provider` example](../packages/bcode/examples/scripted_provider.rs) and focused
[integration tests](../packages/bcode/tests/scripted_provider.rs).

## Additional application test doubles

The testing feature also supplies composable fixtures for SDK boundaries beyond model providers:

* `ScriptedTool` registers a finite scoped inline-tool script, captures canonical invocation
  descriptors, supports complete responses, application failures, Tokio-clock delays, and pending
  work that terminates through normal invocation cancellation.
* `ScriptedPermissionPolicy` consumes typed allow/ask/deny decisions and captures complete canonical
  permission requests.
* `ScriptedModelResponseCache` captures get/put/abort/invalidate order, can preload responses,
  configures privacy/tool replay behavior, and injects typed operation failures.
* `ScriptedSessionStore` preloads and captures complete versioned session payloads and injects load
  or save failures through the public persistence adapter.
* `ManualClock` provides explicit monotonic time, manual advancement, and async sleeps without wall
  clock waits. Provider and tool delays continue to use Tokio's clock so applications may instead
  use Tokio's paused-time support when testing those runtime-owned paths.

These fixtures implement or feed the same public adapters and builder extension points used by
applications. They require no network, credentials, native plugins, daemon, or TUI. See
[`testing_doubles`](../packages/bcode/tests/testing_doubles.rs) for composition and failure examples.

## Stream recording and assertions

`TextStreamRecorder` incrementally records high-level `TextStreamItem` values. Tests may consume one
or a bounded number of items, inspect a genuine partial transcript, continue to exhaustion, request
cancellation with the same `CancellationToken`, or deliberately drop the recorder to exercise the
stream's normal drop-cancellation path.

A completed `TextStreamTranscript` exposes exact normalized events, stable event discriminants, and
scoped events. Its assertions cover:

* exact event or event-kind ordering;
* whether partial consumption has reached stream exhaustion;
* exactly one terminal item delivered last;
* successful response completion;
* typed runtime errors and cancellation; and
* exact bounded-buffer overflow capacity.

The recorder does not add buffering or timing of its own. Backpressure tests configure the real
`AgentRuntime` stream capacity, allow the real producer to fill that bounded queue, and then assert
the canonical `RuntimeError::StreamBufferFull`. Dropping a partially consumed recorder drops the
real SDK stream, so provider cancellation and finish behavior remain production behavior rather
than a testing simulation.

See [`stream_testing`](../packages/bcode/tests/stream_testing.rs) for deterministic success,
partial-consumption, cancellation, early-drop, overflow, and malformed-transcript assertions.

## Compile-time API contracts

Trybuild compile-pass and compile-fail suites protect public usage independently of runtime tests.
The feature-enabled suite compiles representative scripted-provider/stream and typed-tool programs,
including explicit `Send`/`Sync` expectations. Compile-fail fixtures prove that non-`Send`/`Sync`
tool handlers are rejected and testing implementation state remains private. A separate
`--no-default-features` suite proves that `bcode::testing` is unavailable unless the opt-in
`testing` feature is enabled.

The expected compiler diagnostics are checked into `packages/bcode/tests/ui`. Update them only
when a reviewed public type-safety or visibility contract intentionally changes; do not overwrite
snapshots merely to hide a regression.

## Executable examples

`scripts/check-sdk-examples.sh` executes every example under `packages/bcode/examples` with its
required feature set. These are runtime acceptance flows, not compile-only snippets: examples use
deterministic local providers, in-memory or temporary persistence, embedded fake plugins, typed
provider extensions, and client construction, so validation needs no credentials or external
network service. Adding an SDK example requires adding it to this script (and declaring any
`required-features` in `packages/bcode/Cargo.toml`) so the taught behavior remains executable.

## Scope

This feature owns deterministic model-provider, tool, permission, cache, session-store, and clock
fixtures plus provider request/lifecycle capture and high-level text-stream recording/assertions.
Evaluation APIs are intentionally separate behind the `evaluation` feature; see
[`sdk-evaluation.md`](sdk-evaluation.md).
