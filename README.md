# Bcode

Bcode is a Rust-native, TUI-first, plugin-driven coding agent with a local client/server architecture.

## Use Bcode as a Rust SDK

The `bcode` crate can be used directly from Rust applications without launching the TUI. The default feature set is intentionally lean and TUI-independent; opt into heavier product or integration paths only when needed.

```toml
[dependencies]
bcode = { version = "0.0.1-alpha.0", default-features = false }
```

Basic stateless text generation uses explicit imports and a provider invoker:

```rust,no_run
use bcode::{Agent, ModelProviderInvoker};

# async fn run(mut provider: impl ModelProviderInvoker) -> bcode::Result<()> {
let agent = Agent::builder().model("example-model").build();
let response = agent.run(&mut provider, "Say hello").await?;
println!("{}", response.text);
# Ok(())
# }
```

### Builder-first quickstart

Dedicated request builders are the primary API. They keep advanced options discoverable while still accepting any caller-supplied `ModelProviderInvoker`:

```rust,no_run
use bcode::{ModelProviderInvoker, generate_text_builder};
use std::time::Duration;

# async fn run(mut provider: impl ModelProviderInvoker) -> bcode::Result<()> {
let response = generate_text_builder()
    .model("example-provider:example-model")
    .system("Answer concisely.")
    .prompt("Why is the sky blue?")
    .metadata("request-id", "demo-1")
    .timeout(Duration::from_secs(30))
    .run(&mut provider)
    .await?;
println!("{}", response.text);
# Ok(())
# }
```

For the smallest stateless call, use the thin helper; it delegates to the same builder:

```rust,no_run
# use bcode::ModelProviderInvoker;
# async fn run(mut provider: impl ModelProviderInvoker) -> bcode::Result<()> {
let response = bcode::generate_text(&mut provider, "Say hello").await?;
# Ok(())
# }
```

Progressively add `stream_text_builder`, `generate_object_builder::<T>`, `stream_object_builder::<T>`, tools, hooks, or sessions without changing the provider/model concepts.

### Runtime modes and features

* With `default-features = false`, the lean SDK includes builders, caller-supplied providers, tools, hooks, structured output, and lightweight session APIs; it does not include config loading, daemon IPC, the TUI, OCR, or bundled plugins.
* Enable `config` to load provider/model defaults from Bcode's layered config and environment rules.
* Enable `embedded-plugins` to host plugin-backed providers and tools directly in-process; this does not require daemon IPC.
* Enable `daemon-client` for the separate `BcodeClient` daemon path; it does not enable the TUI.
* Enable `app` only for the complete Bcode CLI/TUI product and its bundled plugin/OCR packaging. Individual static bundled plugin features remain opt-in for custom embedded distributions.

### Provider registry and defaults

Provider selection uses `ModelSelector`; `"provider:model"` selects both values, while `"model"` leaves provider routing to the configured invoker. Explicit defaults stay lean:

```rust
use bcode::{Bcode, ProviderRegistry};

let registry = ProviderRegistry::new()
    .provider("example-provider")
    .default_model("example-provider:example-model");
let sdk = Bcode::builder().provider_registry(registry).build();
assert_eq!(sdk.default_model_selector().unwrap().model_id(), "example-model");
```

With the `config` feature, `Bcode::configured()` or `Bcode::builder().load_provider_defaults()` reads the normal layered Bcode configuration. `provider_defaults_from_config(...)` accepts an already loaded config, and `provider_defaults_from_config_environment(...)` supports deterministic environment snapshots. Configured defaults select a provider/model; actual embedded execution additionally requires `embedded-plugins` plus `plugin_runtime(...)`.

Environment resolution follows the application rules, including `BCODE_MODEL_PROVIDER`/`BCODE_PROVIDER`, provider-specific model variables such as `BCODE_OPENAI_MODEL` or `BCODE_BEDROCK_MODEL`, and credential-based provider detection. Explicit builder selections can override defaults.

For tests, examples, or custom integrations, implement `ModelProviderInvoker` and run a request builder. When a provider ends a round with tool calls, the runtime executes the complete batch and returns results in provider order until the provider finishes. Provider factories can instead be configured on `AgentBuilder`. Embedded applications create a plugin runtime host and pass it through `Bcode::builder().plugin_runtime(...)` or `Agent::builder().plugin_runtime(...)`.

### Custom tools

Register synchronous inline tools with `Agent::builder().inline_tool(...)`; handlers receive a transport-free `ToolInvocationDescriptor`. For Rust-typed inputs and outputs, use `TypedTool::<Input, Output>::new(...)` with `typed_tool(...)`: Bcode derives the input JSON Schema, decodes provider arguments into `Input`, and serializes `Output` into a structured JSON tool result. Inline tools are best for application-local behavior that should run in the embedding process. Use `tool_choice(ToolChoice::Auto | None | Required | Tool { .. })` to control provider tool selection where supported. Inline handler failures fail the turn by default; `tool_failure_policy(ToolFailurePolicy::ReturnToModel)` converts them into model-visible error results for recovery. Use `scoped_inline_tool(...)` for asynchronous local tools that also need exchanges, unsolicited input, nested services, artifact writes, lifecycle events, contributions, or cancellation through `InvocationScope`.

Use plugin-backed tools when behavior should be discoverable from plugin manifests, independently packaged or disabled, permission/capability described by the plugin owner, or shared with the Bcode product. Enable `embedded-plugins`, construct a `PluginRuntimeHost`, and pass it to `Bcode::builder().plugin_runtime(...)`. `Bcode::discover_tools()` queries only manifests declaring `bcode.tool/v1` and returns plugin-owned definitions plus routing IDs; register selected definitions with `plugin_tool(...)`. Custom distributions can enable individual `static-bundled-*-plugin` features; `bundled-plugins` exposes bundled registrations without implicitly enabling the complete app, while `app` is reserved for full CLI/TUI packaging. Both inline and plugin tools retain `bcode_tool::ToolDefinition` compatibility, and provider-requested batches use the same registered tools and execution options as direct calls.

Advanced hosts can inject typed adapters without implementing orchestration:

```rust,no_run
use bcode::{
    Agent, HeadlessExchangePolicy, InvocationArtifactSink, InvocationInputRouter,
    InvocationServiceRouter, ToolInvoker,
};
use std::sync::Arc;

# fn build(
#     invoker: Arc<dyn ToolInvoker>,
#     inputs: Arc<dyn InvocationInputRouter>,
#     services: Arc<dyn InvocationServiceRouter>,
#     artifacts: Arc<dyn InvocationArtifactSink>,
# ) -> Agent {
Agent::builder()
    .tool_invoker(invoker)
    .input_router(inputs)
    .service_router(services)
    .artifact_sink(artifacts)
    .headless_exchange_policy(HeadlessExchangePolicy::Reject)
    .build()
# }
```

### Multi-step generation metadata

Every completed `GenerateTextResponse` includes ordered `GenerationStep` values in addition to the raw runtime events. `Model` steps identify the zero-based provider round and aggregate its text, reasoning, and usage; `ToolCall` and `ToolResult` preserve model/runtime boundaries; `FinalResponse` records final text, stop reason, and total latency. This gives application code a stable step-oriented view while retaining `response.runtime.events` for event-level diagnostics.

### Streaming events

Use `Agent::stream(provider, prompt)` for a complete provider/tool turn. It yields `ScopedAgentStreamItem` values whose events are generic `ScopedTurnEvent` runtime, invocation-lifecycle, or contribution envelopes, followed by exactly one final response or error as the last stream item. Provider events, including tool-call requests, retain provider occurrence order. Concurrent tool lifecycle, contribution, and `ToolResult` stream events may interleave or arrive in completion order and must be correlated by invocation/call ID and sequence. After the complete batch settles, tool-result messages sent to the next provider round and returned batch outputs are restored to provider call order. `Agent::stream_text_with_provider(...)` is the high-level provider-only text stream; advanced hosts can call `AgentRuntime::run_streaming_text_turn(...)` when they explicitly need the raw runtime `AgentStream`.

`TextStream`, `AgentStream`, `ScopedAgentStream`, and `ObjectStream<T>` implement `futures::Stream`, so applications can use standard stream combinators by importing `futures::StreamExt`. Each type also retains an inherent `next()` convenience method. `TextStream` is the high-level SDK text stream with typed `BcodeError` and terminal middleware/hook finalization; `AgentStream` is the advanced raw runtime stream. Dropping a live runtime-backed stream requests cancellation of its complete provider turn; normal terminal delivery marks the stream complete before the terminal item is sent.

Runtime-created streams use a bounded queue of `DEFAULT_STREAM_BUFFER_CAPACITY` items (256 by default). Configure a different positive bound with `AgentRuntime::with_stream_buffer_capacity(...)`. If a consumer falls behind and fills the queue, Bcode cancels the provider turn and terminates with `RuntimeError::StreamBufferFull` instead of buffering without limit. Terminal results and errors use a separate single terminal slot, so overflow reporting cannot itself block behind the full event queue.

### Streaming and completed-response semantics

Successful runtime events are exposed live in occurrence order and retained verbatim in the terminal `GenerateTextResponse::runtime.events`, so late consumers and non-streaming calls retain warnings, usage, exact-input counts, request projections, provider metadata, retry notices, reasoning, tool events, and the final lifecycle event. `runtime.usage` and `runtime.stop_reason` mirror the final provider snapshot. `GenerationStep` provides the stable provider-ordered model/tool/final view; completion-order concurrent tool events remain available in the raw event list.

Failures are terminal typed `BcodeError`/`RuntimeError` values rather than synthetic successful responses. Streaming consumers keep events already observed before a provider or middleware failure; non-streaming callers receive the same structured cause. Provider-emitted cancellation first produces `AgentEvent::Cancelled` and then terminal `RuntimeError::Cancelled`; cancellation before provider startup has no fabricated provider event and returns the same terminal error.

### Structured output

Use `generate_object_builder::<T>()` for serde-typed extraction. `StructuredOutputOptions::for_type::<T>()` derives a JSON Schema from `schemars`; `StructuredOutputOptions::json_schema(...)` accepts explicit schemas. Bcode requests provider-native structured output where available, validates returned JSON locally, and supports explicit repair attempts with `with_max_repairs(...)`.

For incremental UI updates, use `stream_object_builder::<T>()`. `ObjectStreamItem::RawDelta` contains each raw fragment. `Partial` contains a changed best-effort JSON value reconstructed only from valid incomplete prefixes; syntactically invalid prefixes are not repaired. `ValidatedPartial` is emitted when a changed partial value passes the configured schema. `Finished` contains the final schema-validated, decoded `T` plus the complete response, while `Error` is terminal for provider, timeout, cancellation, JSON, schema, and decode failures. Schemas that cannot be satisfied incrementally simply produce raw/partial events without `ValidatedPartial` until a satisfying value exists; if the final value never satisfies the schema, the stream ends with `Error`. Streaming repair attempts are rejected explicitly because retrying after visible deltas would require retracting prior events; use buffered `generate_object_builder::<T>()` when `with_max_repairs(...)` is required. The `stream_object(...)` helper delegates to the same builder.

### Middleware and reliability

Non-streaming text and structured requests support transport-independent `ModelMiddleware`. A middleware layer receives the complete `AgentTurnRequest` before provider invocation and the `GenerateTextResponse` after success. Layers run in registration order before the request and unwind in reverse order afterward:

```rust,no_run
use bcode::{AgentTurnRequest, GenerateTextResponse, ModelMiddleware, ModelProviderInvoker};

struct RedactAndBudget;

impl ModelMiddleware for RedactAndBudget {
    fn before_request(&self, mut request: AgentTurnRequest) -> bcode::Result<AgentTurnRequest> {
        if request.prompt.len() > 8_000 {
            return Err(bcode::BcodeError::Hook("request budget exceeded".into()));
        }
        request.prompt = request.prompt.replace("secret", "[redacted]");
        Ok(request)
    }

    fn after_response(
        &self,
        _request: &AgentTurnRequest,
        response: GenerateTextResponse,
    ) -> bcode::Result<GenerateTextResponse> {
        // Inspect response.runtime.usage, latency_ms, warnings, and events here.
        Ok(response)
    }
}

# async fn run(mut provider: impl ModelProviderInvoker) -> bcode::Result<()> {
let response = bcode::generate_text_builder()
    .prompt("Do not reveal this secret")
    .middleware_layer(RedactAndBudget)
    .fallback_policy(
        bcode::FallbackPolicy::new().fallback("backup-provider:backup-model"),
    )
    .run(&mut provider)
    .await?;
# Ok(())
# }
```

Middleware can implement rate-limit/budget rejection, redaction and safety transforms, response auditing, and tracing without server or TUI dependencies. For buffered response caching, implement `ModelResponseCache` and attach it with `response_cache(...)`; the application owns key construction, expiration, capacity, and storage, while Bcode performs lookup after request middleware and stores successful non-streaming provider responses before response middleware. `RetryPolicy` retries only provider-originated failures through the runtime's cancellation-aware retry boundary; `FallbackPolicy` instead switches through an ordered list of provider/model selectors. Configure one planner directly when custom behavior must combine retry, fallback, compaction, or request rebuilding. Timeout, cancellation, permission, tool, middleware, cache, and validation failures remain terminal by default.

High-level `TextStream`, `ObjectStream<T>`, and `ScopedAgentStream` use the canonical provider/tool loop, so configured tools and provider round planners—including retry and fallback policies—remain active. They apply request middleware before provider startup and response middleware plus model hooks to the terminal response. `TextStreamItem::ScopedEvent` retains invocation lifecycle and contribution events that do not fit the normalized model-event family. Response transforms do not buffer, rewrite, or retract already emitted deltas; transformed terminal `response.text` is canonical and Bcode resynchronizes `response.runtime.text` plus ordered `response.steps` after the middleware stack. A response-middleware rejection becomes the single terminal typed SDK error after any visible events. Streaming intentionally bypasses `ModelResponseCache`: replaying a cached completed response would not reproduce trustworthy event timing or provider/tool lifecycle. Low-level `AgentStream` remains available as the raw runtime stream when SDK middleware, hooks, tools, and provider-round planning are not wanted.

### Hooks and observability

`AgentBuilder` supports `on_before_model`, `on_after_model`, `on_before_tool`, and `on_after_tool` hooks. Hook contexts expose model IDs, prompts, tool calls, metadata, latency, and runtime events so applications can add logging, metrics, policy checks, or tracing without depending on TUI internals.

### Optional sessions and persistence

Stateless calls do not require a session. For frontend-oriented conversations, call `Agent::chat()` and `AgentSession::send(...)`; `session()` and `generate_text_with_provider(...)` remain the explicit equivalents. The chat/session wrapper exposes the visible transcript, append, retry/regenerate, branch/fork, and import/export operations.

Applications can attach memory, retrieval, summaries, or profile context through `SessionContextProvider`. Context providers return normal `ModelMessage` values for each request, but those injected messages are not appended to or persisted with the visible transcript. For extensible SDK-managed persistence, implement `SessionPersistenceAdapter` and call `Agent::session_with_persistence(...)`; adapters load/save complete `PersistedSession` values and can target databases, object stores, or application services without TUI/server dependencies. `LocalSessionStore` is the built-in explicit JSON adapter and remains available through `session_with_store(...)`. Missing stores start empty, while empty or corrupt stores return repair/replacement errors instead of silently rebuilding or replaying unbounded history.

Daemon-backed session catalogs, attach/history operations, model selection, input submission, and cancellation remain intentionally `bcode_client`-only. Enable `daemon-client` and use the re-exported `BcodeClient`; embedded `AgentSession` persistence adapters do not silently connect to or depend on a daemon.

See `packages/bcode/examples/` for runnable examples covering text generation, streaming, custom tools, hooks/observability, structured output, local sessions, and daemon-client setup.

## TUI keybindings

TUI keybindings are configurable in `bcode.toml` under scoped `[tui.keybindings.*]` tables. Each scope maps `key = "action.id"`, matching bmux-style key-to-action configuration. Set a key to `""`, `"none"`, or `"unbind"` to remove a default binding for that key.

```toml
[tui.keybindings.chat]
"enter" = "tui.input.submitSteering"
"ctrl+shift+enter" = "tui.input.submitFollowUp"
"shift+enter" = "tui.input.newLine"
"up" = "tui.input.historyPrevious"
"down" = "tui.input.historyNext"
"left" = "tui.editor.moveCursorLeft"
"right" = "tui.editor.moveCursorRight"
"alt+left" = "tui.editor.moveCursorWordLeft"
"alt+right" = "tui.editor.moveCursorWordRight"
"ctrl+left" = "tui.editor.moveCursorWordLeft"
"ctrl+right" = "tui.editor.moveCursorWordRight"
"ctrl+a" = "tui.editor.moveCursorStart"
"ctrl+e" = "tui.editor.moveCursorEnd"
"backspace" = "tui.editor.deleteCharBackward"
"delete" = "tui.editor.deleteCharForward"
"alt+backspace" = "tui.editor.deleteWordBackward"
"ctrl+w" = "tui.editor.deleteWordBackward"
"alt+delete" = "tui.editor.deleteWordForward"
"ctrl+delete" = "tui.editor.deleteWordForward"
"ctrl+u" = "tui.editor.deleteToStart"
"ctrl+k" = "tui.editor.deleteToEnd"
"escape" = "app.interrupt"
"ctrl+d" = "app.exit"
"ctrl+c" = "app.clear"
"ctrl+f" = "app.search"
"pageUp" = "transcript.pageUp"
"pageDown" = "transcript.pageDown"

[tui.keybindings.permission]
"y" = "app.permission.approve"
"n" = "app.permission.deny"
"a" = "app.permission.alwaysAllow"
"d" = "app.permission.alwaysDeny"
"left" = "tui.select.previous"
"right" = "tui.select.next"
"enter" = "tui.select.confirm"
"escape" = "tui.select.cancel"

[tui.keybindings.session_picker]
"up" = "tui.select.previous"
"down" = "tui.select.next"
"enter" = "tui.select.confirm"
"escape" = "tui.select.cancel"
```

Key format follows `modifier+key`, with `ctrl`, `alt`, and `shift` modifiers. Examples: `ctrl+d`, `alt+left`, `pageUp`, `escape`, `enter`.

The chat composer uses a Unicode-aware editor buffer. Standard composer defaults include `up` / `down` session message history navigation, left/right grapheme movement, `alt+left` / `alt+right` and `ctrl+left` / `ctrl+right` word movement, `ctrl+a` / `ctrl+e` start/end movement, `backspace` / `delete` character deletion, `alt+backspace` / `ctrl+w` word-backward deletion, `alt+delete` / `ctrl+delete` word-forward deletion, and `ctrl+u` / `ctrl+k` delete-to-start/end. Plain `home` and `end` remain transcript top/bottom bindings in the chat scope by default.

Permission prompts are modal by default: permission actions only apply in the permission scope, and hints are generated from the configured permission keymap.

### Permissions

Bcode uses an agent-scoped permission model with `allow` / `ask` / `deny` rules under `[agent.<id>.permission]` in `bcode.toml`. See [`docs/permissions.md`](docs/permissions.md) for the full shape, category list, and built-in defaults for the `plan` and `build` agents.

### Plugin and tool selection

Bcode separates plugin loading from model-callable tool exposure. Statically bundled plugins are compiled into the Bcode binary, but users can still opt out of bundled defaults or opt in to individual plugins.

```toml
[plugins]
default = "none"
enabled = ["bcode.default-agents", "bcode.filesystem", "bcode.vim-edit"]

[tools]
default = "none"
enabled = ["filesystem.read", "vim_edit.preview"]
```

Use `default = "bundled"` under `[plugins]` to enable Bcode's bundled defaults unless disabled, `default = "none"` to start with no default plugins, or `default = "all"` to enable every discovered plugin unless disabled. Under `[tools]`, `default = "agent"` uses the active agent's normal tool policy, `default = "none"` exposes only explicitly enabled tools, and `default = "all"` exposes all loaded tools except those in `disabled`.

```toml
[plugins]
disabled = ["bcode.vim-edit"]

[tools]
disabled = ["vim_edit.apply"]
```

## Auth vault device seals

Bcode stores provider secrets in sshenv-backed auth vault profiles. By default, Bcode prefers a strict transparent device-only seal for those profiles: macOS uses a non-syncing `ThisDeviceOnly` Keychain item, Windows uses current-user DPAPI, and Linux uses TPM when available. If the seal cannot be applied and `device_seal = "preferred"`, Bcode continues with a warning; `device_seal = "required"` turns that into an error.

Advanced auth profile settings can override the default:

```toml
[auth.profiles.openai.settings]
device_seal = "preferred"              # off, preferred, required
device_seal_mode = "transparent-device-only" # transparent-device-only, default
device_seal_strict = "true"
# Optional explicit backend override:
# device_seal_backend = "macos-keychain-device-only"
```

Run `bcode auth status` to inspect the configured mode and the backend recorded in vault metadata.

## Session import

Bcode can discover sessions from other coding agents through bundled session-import plugins. The Pi importer is enabled by default and reads Pi JSONL history without mutating Pi's files.

In the TUI, open the session picker or run `/rescan-imports`; importable rows are marked like `[pi import]`. Selecting one copies it into a normal Bcode session and continuation uses Bcode's selected provider, agent, tools, and permissions. Imported external tool calls are inert history and are not replayed.

CLI helpers:

```sh
bcode session import sources
bcode session import discover --source pi
bcode session import discover --source pi --diagnostics
bcode session import open --source pi <external-session-id>
```

Configuration lives under `[session_import]` and `[session_import.pi]`:

```toml
[session_import]
enabled = true
hide_already_imported = true

[session_import.pi]
enabled = true
path_mode = "defaults_and_custom" # defaults_only, custom_only, defaults_and_custom
paths = ["/path/to/pi/sessions"]
```

The default Pi path is `~/.pi/agent/sessions`. Use `custom_only` to avoid scanning the default home-directory location. Import warnings are shown when mappings are lossy, such as image blocks that are not yet copied into Bcode artifacts.

### Client request timeout

Local client/daemon IPC requests time out after 15 seconds by default. For slower
session opens, set a persistent override in `bcode.toml`:

```toml
[client]
request_timeout_secs = 60
```

Use `bcode --request-timeout-secs 60 ...` for a one-shot override. This setting
controls local IPC requests; it does not change model-provider HTTP timeouts.
