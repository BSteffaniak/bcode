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

### Runtime modes and features

* Embedded mode runs in-process and does not require daemon IPC. Enable `embedded-plugins` when the application wants to host plugin-backed providers/tools itself.
* Daemon mode is a separate programmatic path. Enable `daemon-client` to use `BcodeClient` through the SDK facade without pulling TUI code into the default library path.
* The CLI/TUI product is behind the `app` feature, which enables the full CLI/TUI and bundled-plugin feature set. Library users should keep `default-features = false` unless they intentionally want product packaging.
* Bundled OCR and static bundled plugin features are opt-in through the `bcode` crate feature flags used by the app package.

### Providers

For tests, examples, or custom integrations, implement `ModelProviderInvoker` and call `Agent::run`. When a provider ends a round with one or more tool calls, `run` automatically executes the complete batch and sends results back in provider order until the provider finishes. Provider factories can instead be configured on `AgentBuilder` for `Agent::generate_text` and `Agent::stream_text`. Plugin-backed embedded applications can create a plugin runtime host and pass it through `Agent::builder().plugin_runtime(...)` with the `embedded-plugins` feature enabled.

### Custom tools

Register synchronous inline tools with `Agent::builder().inline_tool(...)`; handlers receive a transport-free `ToolInvocationDescriptor`. Use `scoped_inline_tool(...)` for asynchronous tools that also need exchanges, unsolicited input, nested services, artifact writes, lifecycle events, contributions, or cancellation through `InvocationScope`. Tool definitions use `bcode_tool::ToolDefinition`. Provider-requested batches use the same registered tools and configured execution options as direct calls.

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

### Streaming events

Use `Agent::stream(provider, prompt)` for a complete provider/tool turn. It yields `ScopedAgentStreamItem` values whose events are generic `ScopedTurnEvent` runtime, invocation-lifecycle, or contribution envelopes, followed by a final response or error. `Agent::stream_text_with_provider(...)` remains the provider-only compatibility stream.

### Structured output

Use `Agent::generate_object_with_provider::<T, _>(...)` for serde-typed extraction. `StructuredOutputOptions::for_type::<T>()` derives a JSON Schema from `schemars`; `StructuredOutputOptions::json_schema(...)` accepts explicit schemas. Bcode requests provider-native structured output where available, validates returned JSON locally, and supports explicit repair attempts with `with_max_repairs(...)`.

### Hooks and observability

`AgentBuilder` supports `on_before_model`, `on_after_model`, `on_before_tool`, and `on_after_tool` hooks. Hook contexts expose model IDs, prompts, tool calls, metadata, latency, and runtime events so applications can add logging, metrics, policy checks, or tracing without depending on TUI internals.

### Optional sessions and persistence

Stateless calls do not require a session. For in-memory conversations, call `Agent::session()` or `Agent::session_from_messages(...)`; transcripts can be exported with `InMemorySession::into_messages()` for caller-managed persistence. For explicit local JSON persistence, use `LocalSessionStore` with `Agent::session_with_store(...)`. Missing stores start empty, while empty or corrupt stores return repair/replacement errors instead of silently rebuilding or replaying unbounded history.

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
