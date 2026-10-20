# Rust SDK

The `bcode` crate is the application-facing facade for Bcode's provider, agent, tool, session, and workflow capabilities. It can run entirely in-process without the terminal UI or local daemon.

Bcode is not yet published on crates.io. Depend on the Git repository and pin a commit for reproducible application builds:

```toml
[dependencies]
bcode = { git = "https://github.com/BSteffaniak/bcode", rev = "<commit>", default-features = false }
```

## Generate text

The lean SDK accepts any caller-provided `ModelProviderInvoker`:

```rust,no_run
use bcode::{ModelProviderInvoker, generate_text_builder};

async fn generate(provider: &mut impl ModelProviderInvoker) -> bcode::Result<String> {
    let response = generate_text_builder()
        .model("provider:model")
        .system("Answer concisely.")
        .prompt("Explain this patch")
        .run(provider)
        .await?;

    Ok(response.text)
}
```

`Agent::builder()` provides a reusable configured agent when an application needs tools, policy, hooks, sessions, or repeated calls. `AgentBuilder::from_context(session_id, cwd)` accepts explicit initialization inputs without generating a session ID or reading the process working directory; use an absolute directory to avoid later relative-path resolution. This is not a deterministic-execution guarantee. `InProcessModelProviderAdapter` is the shortest provider implementation path: applications implement one asynchronous turn while Bcode handles provider turn IDs, polling, terminal lifecycle, cancellation races, and cleanup.

Executable examples:

* [`minimal_text`](../packages/bcode/examples/minimal_text.rs)
* [`custom_provider`](../packages/bcode/examples/custom_provider.rs)
* [`top_level_helpers`](../packages/bcode/examples/top_level_helpers.rs)

## Streaming and structured output

`Bcode::builder().provider_factory(...)` supplies caller-owned provider acquisition to all
agents created by the SDK, including `agent_from_context`. It needs no plugin-host feature.
Each generation or stream acquires a fresh invoker; building and cloning the SDK do not
invoke the factory. Agent-level factories override the inherited factory. Factories take
precedence over plugin-backed providers without replacing tool discovery or model selection.
Acquisition effects and captured state remain the caller's responsibility.

Applications that own provider request correlation IDs can configure
`AgentRuntime::with_provider_request_identity_source(Arc<dyn ProviderRequestIdentitySource>)`
and pass that runtime to `AgentBuilder::runtime`. The source supplies a `ProviderRequestIdentity`
for each request before provider startup; allocation errors prevent that startup. Runtime clones
share the source. Callers must provide distinct request identities and control request ordering
for reproducibility. These correlation IDs do not select canonical session storage. Without a
source, the runtime retains its random per-request identity behavior. With the `testing`
feature, `testing::ScriptedRequestIdentities` supplies a finite validated sequence, rejects
empty turn IDs and duplicate session/turn pairs, and fails on exhaustion. Its clones share
consumption; construct a fresh source for each run.

`stream_text_builder` and agent streaming methods expose standard `futures::Stream` implementations while retaining normalized runtime and plugin-invocation events.

`generate_object_builder::<T>` and `stream_object_builder::<T>` derive JSON Schema from Rust types, decode provider output, and support bounded repair attempts. See the executable [`structured_output`](../packages/bcode/examples/structured_output.rs) example.

## Tools and permissions

Applications can register local inline tools or use typed tools whose Rust input generates a JSON Schema and whose output becomes structured model-visible data. The same runtime handles provider-requested tool batches, permission decisions, cancellation, and round limits.

Use plugin-backed tools when behavior should be manifest-discoverable, independently packaged or disabled, or shared with the Bcode product. Embedded applications can provide a `PluginRuntimeHost` and ask `Bcode` to discover manifest-declared tool services.

Examples:

* [`custom_tool`](../packages/bcode/examples/custom_tool.rs)
* [`multi_step_tools`](../packages/bcode/examples/multi_step_tools.rs)
* [`embedded_fake_provider`](../packages/bcode/examples/embedded_fake_provider.rs)

## Sessions, persistence, and memory

`AgentSession` is an explicit stateful conversation wrapper. Successful turns retain the complete visible assistant, tool-call, and tool-result transcript needed by future requests. Sessions support regeneration, branching, import/export, application-defined persistence, and a small local JSON store.

Retrieved application memory is request-only by default. Persistence requires an explicit `remember` operation, so retrieval does not silently mutate durable conversation state.

See:

* [Stateful chat semantics](stateful-chat.md)
* [Application memory](application-memory.md)
* [`in_memory_session`](../packages/bcode/examples/in_memory_session.rs)
* [`local_session_store`](../packages/bcode/examples/local_session_store.rs)

## Middleware, reliability, and observability

The SDK supports:

* request and response middleware;
* model and tool hooks;
* cancellation and timeouts;
* provider retry and ordered fallback policies;
* application-owned rate limiting;
* completed-response caching with explicit privacy and replay rules;
* `tracing` spans without installing a subscriber;
* optional metrics and OpenTelemetry adapters.

Detailed contracts:

* [Retry and rate-limit handling](rate-limit-handling.md)
* [Model response caching](model-response-cache.md)
* [SDK tracing](sdk-tracing.md)
* [SDK evaluation](sdk-evaluation.md)

## Frontends and daemon clients

The public frontend event and snapshot contracts are renderer-neutral. Terminal, web, desktop, and service applications can consume normalized session semantics without depending on TUI or daemon-private types.

Enable `daemon-client` to use `BcodeClient` with a local Bcode daemon. This is separate from embedded execution and does not enable the TUI. See [Frontend contracts](frontend-contracts.md) and the [`daemon_client`](../packages/bcode/examples/daemon_client.rs) example.

## Portable application operations

Daemon-backed Rust consumers use domain-owned traits implemented by `BcodeClient`,
including session/runtime-work and observation, workflow run/authoring/observation,
plugin services and interactions, typed commands, worktree, model/agent/skill, usage,
import/search, auth-pool and runtime-lifecycle access. See the
[operation boundary](application-operation-boundary.md) and
[parity inventory](application-operation-parity.md) for implemented paths and gaps;
trait availability alone does not certify CLI or renderer parity.

### Complete session consumer

[`application_session.rs`](../packages/bcode/examples/application_session.rs) contains
an implemented consumer of `SessionApplication`, `SessionObservationApplication` and
`InteractionApplication`: create, select an agent/model, admit a turn, observe bounded
semantic snapshots, deny requested permissions without remembering decisions, apply
explicit caller-supplied controller inputs with revision fencing, cancel an exact turn
on supervision failure, and spawn/supervise a canonical child. It does not construct a
server or open persistence. It does not change the ordinary low-level generation API.

Use the owning domain crates for traits; `daemon-client` does not re-export every
application contract. For development against this **uncommitted** implementation, the
honest dependency recipe is a checkout path, not a published version or an older Git
commit that lacks these APIs:

```toml
[dependencies]
bcode = { path = "/absolute/path/to/bcode/packages/bcode", default-features = false, features = ["daemon-client"] }
bcode_session_models = { path = "/absolute/path/to/bcode/packages/session/models", default-features = false }
bcode_plugin_sdk = { path = "/absolute/path/to/bcode/packages/plugin-sdk", default-features = false }
serde_json = { version = "1.0.149", default-features = false, features = ["std"] }
tokio = { version = "1.45.1", default-features = false, features = ["macros", "rt-multi-thread", "time"] }
```

The example's imports name the exact modules. For a reproducible Git consumer, replace
**all three** path entries with `git = "https://github.com/BSteffaniak/bcode"` and the
**same full `rev` containing this implementation**, and commit the consumer's lockfile.
There is not yet a published/pinned source revision for the working-tree additions;
`6a24977fe468f36db8c821bbb870ad29ec047199` is the base, not a sufficient API pin.
Upstream workspace patches are not inherited by a Git dependency; reproduce any
required upstream patches deliberately in the consumer root rather than assuming
Bcode's development checkout configuration is portable.

**Explicit independent-process targeting.** Use
`DaemonTarget::verify(executable, expected_sha256, location, config_directory).await`,
then `BcodeClient::for_daemon(target, &config, absolute_working_directory)`. The
lifecycle domain verifies the absolute executable's bytes before and after its bounded
`artifact-id` probe, selects one exact running daemon in the state/configuration scope,
and verifies live artifact, executable hash, namespace, protocol and storage contracts.
The client uses those selected handshake facts on every connection, including retained
controller/observation connections, and rechecks executable bytes and live status.
There is no fallback to the consumer's executable or another state location.
Select a trusted, immutable/content-addressed executable path: repeated pathname hashing
detects ordinary replacement, but is not a claim of adversarial replace-and-restore race
immunity during executable probing. Discovery currently uses a strict bounded registry
inventory (4096 entries, 64 KiB per record); exhaustion or unverifiable foreign records
fails closed rather than guessing that the selected daemon is absent.

This explicit constructor is **RequireRunning-only**, including after availability
policy changes. Start the selected Bcode artifact separately using its supported
lifecycle CLI; the consumer never starts, replaces or stops a daemon. Configuration
is supplied as a resolved `BcodeConfig`; its selected model profile and authentication
are resolved through the existing configuration/provider-auth owners, not daemon
defaults. No private IPC setup, persistence handle or extra hosting framework is needed.
The request-only effective configuration is not written to a configuration file.

For the repository example, supply existing absolute paths and a trusted SHA-256:

```sh
# In another terminal, start the selected daemon if it is not already running.
# These paths are explicit selections, not PATH discovery or consumer executable aliases.
XDG_CONFIG_HOME="$CONFIG_HOME" "$DAEMON_EXE" --config-only "$CONFIG_FILE" \
  --profile "$MODEL_PROFILE" --state-root "$STATE_ROOT" server run

BCODE_STATE_DIR="$STATE_ROOT" XDG_CONFIG_HOME="$CONFIG_HOME" \
  cargo run -p bcode --features daemon-client --example application_session -- \
  "$DAEMON_EXE" "$EXPECTED_SHA256" "$STATE_ROOT" "$CONFIG_DIRECTORY" \
  "$CONFIG_FILE" "$MODEL_PROFILE" "$ABSOLUTE_CWD" build \
  'Reply briefly without tools.' "$ABSOLUTE_INPUTS_JSON_FILE"
```

`CONFIG_DIRECTORY` is `$CONFIG_HOME/bcode` when using `XDG_CONFIG_HOME`; select
the same existing configuration-directory scope used by the daemon.
`CONFIG_FILE` is loaded exclusively and the model profile is explicitly overlaid.
The constructor also requires the consumer's process authentication scope
(`BCODE_STATE_DIR` and `XDG_CONFIG_HOME`) to match the target. This fails closed rather
than reading authentication subscriptions from an unrelated default state root; it
never changes process-global selection on behalf of another concurrent consumer. The
example prints the resolved provider/model before admitting work. Use a trusted input
file containing `[]` for a no-interaction run; actual provider work can incur charges.
A missing/incompatible target fails closed without starting a different daemon. The
external consumer manifest also needs the domain dependencies below (all Bcode paths
or Git revisions must identify the same source tree):

```toml
bcode_config = { path = "/absolute/path/to/bcode/packages/config", default-features = false }
bcode_daemon_lifecycle = { path = "/absolute/path/to/bcode/packages/daemon-lifecycle", default-features = false }
```

The repository example target is gated by `required-features = ["daemon-client"]`;
its config/lifecycle imports use existing development dependencies. Ordinary embedded
or direct low-level generation remains unchanged.

Controller inputs are a bounded JSON array (64 KiB and at most 32 inputs). `[]` means
no interaction resolution is authorized. Inputs must correspond to the plugin-owned
snapshot; the consumer does not invent an answer or grant tool permissions. The
retained controller connection is opened before admission and kept throughout the
turn. Unsupported controllers and second exchanges fail closed; the example cancels
rather than waiting indefinitely. A generic production consumer can present the
plugin-owned schema and accept fresh user decisions instead of a preselected array.

The observation path now exists on `BcodeClient` in
`packages/client/src/session_observation.rs`. `observe_session` returns the portable
semantic envelopes with a server-owned `SessionViewSnapshot`, rather than making a
consumer reinterpret raw event logs. Initial snapshot is sequence zero and precedes
ordered updates. Delivery sequence is local to one observation; source sequence is
not a durable acknowledgement/resume cursor. Update, source-input and snapshot bounds
can terminate observation. Gap, invalid source, limit and disconnect are explicit
terminal reasons; no subsequent snapshots are delivered. Resync opens a fresh bounded
observation, not replay of a previous stream. Dropping/closing releases observation
resources without cancelling execution; cancelling a `recv` future retains its pending
pull. The older `watch_session` APIs remain separate event/projection adapters.

The example limits supervision to 30 seconds and requests exact-turn cancellation on
timeout/error, then observes canonical completion for at most five more seconds.
Cancellation acknowledgement is not terminal completion or rollback. A bounded history
page that lacks the exact `ModelTurnFinished` leaves outcome unknown; it does not
fabricate success. Admission or spawn failure can follow committed effects: retain
session/child identity and inspect before retrying. Existing/deferred receipts are
supervised, not resubmitted. Disconnect never authorizes replay of controller inputs.

`packages/bcode/tests/application_complete_path.rs` supplies persistent IPC lifecycle,
controller and observation acceptance cases. The explicit-target domain/client tests
cover identity, hash, state/configuration separation and handshake propagation. These
cases do not certify every CLI/renderer path or replace an independent-process runtime
acceptance run. The `independent_explicit_daemon_consumer_process` test in
`application_complete_path.rs` builds the real Bcode binary as a distinct Cargo artifact,
then runs an env-cleared consumer test process against a copied, hash-recorded test
image in isolated state/configuration. It loads only fake-provider/default-agent/question
plugins, exercises retained controller/semantic observation and canonical lifecycle, and
rejects mismatched hash/state/configuration/artifact and replaced executable bytes.
It has inner/outer watchdogs, graceful shutdown and fixture-child-only kill/reap cleanup.
Run it with no provider credentials or active-session management:

```sh
cargo test -p bcode --test application_complete_path \
  --features app,daemon-client,static-bundled-fake-provider-plugin,static-bundled-default-agents-plugin,static-bundled-question-plugin \
  -- independent_explicit_daemon_consumer_process --exact --nocapture
```

Cargo supplies `CARGO_BIN_EXE_bcode`; no PATH binary, installed runtime or guessed test
artifact ID is used. Defining this acceptance test is not a claim it has passed; report
its observed result separately. See the parity inventory for remaining qualifications.

Usage reporting is query-only; import material loading is not canonical import.
Search reads do not repair indexes. Maintenance and mutations remain explicit, with
confirmation, caller/location checks and domain cancellation. Plugin service callers
must inspect both the outer application result and `PluginServiceResponse::error`;
opaque payload success does not establish command-effect completion. Unknown versions,
unknown failure categories and ambiguous effects must not become permissive defaults.

### Typed commands, cancellation, and native effects

Import `bcode_command::application::{CommandApplication, CommandInvocationRequest,
CommandInvocationOutcome}` alongside `BcodeClient` (add `bcode_command` from
`packages/command` at the same checkout/revision as the other Bcode dependencies).
`list_commands`, `invoke_command`, and `cancel_command` use the typed command owner;
opaque plugin service success is not a substitute for a command outcome.

Retain the same client or a clone for cancellation. Invocation and control share one
caller-bound socket; a separately constructed client cannot cancel another caller's work
by supplying its ID. `cancel_command` returns acceptance, not proof of terminal cancellation,
rollback, or no effects. Keep awaiting the invocation result when possible. Dropping its
future or losing the connection does not establish a safe retry.

Handle every `CommandInvocationOutcome` explicitly:

* `Completed(response)`: inspect the command owner's `success` before applying effects.
* `Failed(error)`: known application failure, not an automatic replay instruction.
* `Unknown(error)`: no trustworthy settled response; effects may have committed. Report
  uncertainty and do not retry automatically or fabricate a terminal failed command.

CLI and TUI consume typed command results and adapt effects natively. Failed commands do
not drive success effects; unknown outcomes supply none. Status/copy presentation is
not implicit canonical persistence. TUI explicitly routes `AppendText` through the canonical
presentation-note operation; CLI emits it without that mutation. Session navigation uses
canonical access, while clipboard,
focus and surfaces remain frontend presentation. CLI emits structured fallback for unsupported
native surfaces rather than claiming they opened. This is source wiring, not certification of
every plugin or renderer. HyperChad's session/interaction support alone does not establish
command invocation/effect parity. Active cancellation, isolation, transport-loss and native-effect
regressions require separately recorded integrated test results; no merge-ready claim follows
from these APIs or test definitions.

`RuntimeApplication` (from `bcode_daemon_lifecycle::application`) provides typed status and
explicit shutdown. Its acknowledgement is admission, not proof of daemon exit or drained work.
The host-only borrowed preparation/commit operation is not a serializable remote token.

## Feature selection

The default feature set is deliberately empty.

| Feature | Adds |
| --- | --- |
| `config` | Bcode's layered provider/model configuration and authentication context |
| `embedded-plugins` | In-process plugin hosting for providers and tools |
| `daemon-client` | The local daemon client API |
| `testing` | Deterministic provider, tool, permission, cache, session, and clock fixtures |
| `evaluation` | Provider-independent result scoring |
| `openai-compatible-provider` | Typed OpenAI-compatible request extensions |
| `app` | The Bcode CLI/TUI binary path |
| `distribution` | Full product packaging: app, bundled plugins/OCR, Mermaid, config, and web renderer |

Individual `static-bundled-*-plugin` features support custom embedded or product distributions without requiring the complete `distribution` composition.

## Deterministic tests

The opt-in `testing` feature includes a finite scripted provider, request/lifecycle capture, stream assertions, and deterministic fixtures. It needs no credentials, network access, native plugins, or daemon.

```toml
[dev-dependencies]
bcode = { git = "https://github.com/BSteffaniak/bcode", rev = "<commit>", default-features = false, features = ["testing"] }
```

See [Deterministic SDK provider tests](sdk-testing.md) and the executable [`scripted_provider`](../packages/bcode/examples/scripted_provider.rs) example.

## Development simulator smoke run

With the currently required local upstream patches, the same example can drive
Switchy's simulator explicitly. It checks a successful response, cancellation after
a provider text delta, and a real agent deadline. Both streaming failures must have
one coherent terminal, exhausted delivery, and exactly one provider cancellation
and finish call. Allowed and denied tool-call scenarios also exercise the real
permission dispatch and provider continuation: denial must invoke no tool, while
allowance must invoke it once with the expected arguments and return its result.

```sh
SIMULATOR_SEED=0 SIMULATOR_EPOCH_OFFSET=1700000000000 SIMULATOR_STEP_MULTIPLIER=1 \
  cargo run --offline -p bcode --no-default-features --features simulation-example --example scripted_provider
```

For separate locked backend builds and a fixed fresh-process campaign with an
external watchdog, run:

```sh
python3 scripts/check-sdk-simulation-smoke.py --offline
```

This runs production once and simulator seeds `0,0,1,2,3,4,5,6,7,8,9` with a fixed
epoch and 1 ms steps. It clears inherited `SIMULATOR_*` overrides, runs the exact
executable reported by Cargo, and fails on build errors, scenario failures, or a
60-second run watchdog (distinct from the simulated product deadline). Each build
has a separate 600-second watchdog. It still requires the development upstream
patches and does not produce a replay bundle or certify host confinement.

`simulation-example` selects the experimental backend; it is not a certified SDK
profile. Each process polls at most 10,000 tasks and advances time by the configured
step multiplier after each unfinished poll. Budget exhaustion is a harness error,
not an application timeout. A large multiplier can legitimately trigger the real
agent deadline. Use a fresh process and an external watchdog: a single blocking
poll is not preemptible. Runtime-wide bounded draining is not yet exposed upstream,
so this smoke run does not establish task cleanup, run isolation, host-effect
confinement, or replay certification. Production execution remains
`cargo run -p bcode --features testing --example scripted_provider`.

## Provider contract

Provider plugins, in-process adapters, and proxies share normalized model semantics. The normative operation, event, capability, cancellation, and error requirements live in the [model provider contract](model-provider-contract.md).
