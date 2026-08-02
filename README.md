# Bcode

**A terminal-native coding agent and Rust SDK built around explicit permissions, durable sessions, and first-class tools.**

Bcode keeps coding work close to the repository. Inspect code, run commands, review changes, manage worktrees, and return to persistent sessions from a keyboard-first terminal interface. Sensitive operations pass through visible permission policy instead of disappearing behind an opaque agent loop.

The same provider, tool, session, and workflow building blocks are available as a lean Rust library for applications that need an agent runtime without the TUI or daemon.

> [!IMPORTANT]
> Bcode is early alpha and currently distributed from source. APIs, configuration, and storage formats may change before the first public release.

## Why Bcode

* **Made for the terminal.** Streamed output, rich Markdown, a command palette, session navigation, and native presentations for tools, diffs, files, and shell commands.
* **Control the work.** Plan and build agents expose different capabilities, while shell commands, file changes, worktrees, and other sensitive operations follow explicit `allow`, `ask`, or `deny` policy.
* **Keep the thread.** Sessions survive the terminal, can be reopened or forked, and can move into isolated Git worktrees when a task needs its own workspace.
* **Extend the agent, not just the prompt.** Providers, tools, authentication, commands, skills, workflows, and rich TUI surfaces can all be plugin-owned.

## Run Bcode

Bcode currently requires the [stable Rust toolchain](https://www.rust-lang.org/tools/install) and a source build:

```sh
git clone https://github.com/BSteffaniak/bcode.git
cd bcode
cargo build --release -p bcode \
  --no-default-features \
  --features app,static-bundled-plugins \
  --bin bcode
./target/release/bcode
```

On Windows, run `target\release\bcode.exe` instead.

The first interactive launch opens Bcode's setup flow for provider, model, authentication, and permission choices. The bundled provider integrations cover OpenAI-compatible APIs—including OpenAI API keys and ChatGPT subscription login—and Amazon Bedrock.

See [Getting started](docs/getting-started.md) for build choices and initial setup, or jump to the [CLI](#documentation) and configuration references.

## Use Bcode from Rust

Until the crate is published, depend on the repository directly. Pin a commit for applications that need reproducible builds.

```toml
[dependencies]
bcode = { git = "https://github.com/BSteffaniak/bcode", default-features = false }
```

Bcode's lean SDK accepts application-owned model providers and does not require the TUI, daemon, bundled plugins, or network access:

```rust,no_run
use bcode::{ModelProviderInvoker, generate_text_builder};

async fn review(provider: &mut impl ModelProviderInvoker) -> bcode::Result<String> {
    let response = generate_text_builder()
        .model("your-model")
        .system("Review the patch. Be precise.")
        .prompt("Review the current changes")
        .run(provider)
        .await?;

    Ok(response.text)
}
```

Add text streaming, typed structured output, tools, middleware, retries, stateful sessions, persistence, memory, or embedded plugins as the application grows. Deterministic provider fixtures support SDK tests without credentials or network access.

Read the [SDK guide](docs/sdk.md) or explore the [executable examples](packages/bcode/examples).

## Extend Bcode

Bcode plugins are native Rust libraries with manifest-declared, versioned interfaces. A plugin can contribute model providers, tools, auth methods, commands, workflow blocks, session importers, visual adapters, and complete terminal surfaces. Bundled plugins use the same boundaries and can be disabled independently.

Start with the [plugin guide](docs/plugins.md) and the [dynamic plugin example](examples/hello-plugin).

## Documentation

### Start here

* [Getting started](docs/getting-started.md)
* CLI and configuration references are generated from Bcode's command tree and Rust schema; [Getting started](docs/getting-started.md#configuration) shows how to build them locally.
* [TUI keybindings](docs/tui-keybindings.md)

### Work with Bcode

* [Permissions](docs/permissions.md)
* [Worktrees](docs/worktrees.md)
* [Skills](docs/skills.md)
* [Session imports](docs/session-import-plugins.md)
* [Reasoning presentation](docs/reasoning-presentation.md)

### Build on Bcode

* [Rust SDK](docs/sdk.md)
* [Plugins](docs/plugins.md)
* [Model provider contract](docs/model-provider-contract.md)
* [Frontend contracts](docs/frontend-contracts.md)

## Project status

Bcode is under active development at `0.0.1-alpha.0`. There are no published binaries or crates yet. Release automation currently targets ARM64 and x86-64 macOS, ARM64 and x86-64 Linux, and x86-64 Windows.

This repository does not yet include a contributor guide or security policy. Use [GitHub Issues](https://github.com/BSteffaniak/bcode/issues) for bug reports and focused proposals; do not include credentials or other sensitive data.

## License

Bcode is available under the [Mozilla Public License 2.0](LICENSE).
