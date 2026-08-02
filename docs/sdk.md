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

`Agent::builder()` provides a reusable configured agent when an application needs tools, policy, hooks, sessions, or repeated calls. `InProcessModelProviderAdapter` is the shortest provider implementation path: applications implement one asynchronous turn while Bcode handles provider turn IDs, polling, terminal lifecycle, cancellation races, and cleanup.

Executable examples:

* [`minimal_text`](../packages/bcode/examples/minimal_text.rs)
* [`custom_provider`](../packages/bcode/examples/custom_provider.rs)
* [`top_level_helpers`](../packages/bcode/examples/top_level_helpers.rs)

## Streaming and structured output

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

## Provider contract

Provider plugins, in-process adapters, and proxies share normalized model semantics. The normative operation, event, capability, cancellation, and error requirements live in the [model provider contract](model-provider-contract.md).
