# Getting started

Bcode is currently an early alpha distributed from source. There are no published binaries or crates yet.

## Prerequisites

Install:

* [Git](https://git-scm.com/)
* the [stable Rust toolchain](https://www.rust-lang.org/tools/install)

Bcode's full distribution build also compiles bundled OCR support and may require a native C/C++ toolchain and CMake. The terminal-agent build below avoids that additional packaging.

## Build the terminal agent

Clone Bcode and build the TUI with its statically bundled providers, tools, commands, and integrations:

```sh
git clone https://github.com/BSteffaniak/bcode.git
cd bcode
cargo xtask build --features static-bundled-plugins
```

The helper includes the `app` feature and assigns a fresh artifact identity for each
build, allowing it to coexist with existing daemon artifacts. Do not substitute a
raw Cargo invocation without following the identity requirements in
[Release builds](release-builds.md).

Run `target/<host-target>/release/bcode`, replacing `<host-target>` with the
`host` value printed by `rustc -vV` (the build's default target). For example, on
Apple Silicon macOS:

```sh
./target/aarch64-apple-darwin/release/bcode
```

On Windows with the MSVC toolchain:

```powershell
.\target\x86_64-pc-windows-msvc\release\bcode.exe
```

The first interactive launch offers a quick connection choice. When a supported provider key is detected, Bcode shows its source, a recommended coding model, and billing guidance; one confirmation remembers the choice and opens the composer. You can instead sign in with a supported subscription, enter an API key, or open Advanced setup.

Bcode remembers nonsecret connection and model references in `<resolved state root>/startup.toml`, not in `bcode.toml`. Environment-backed connections require the selected variable to remain available on future launches; Bcode does not copy that key into the state file. Entered credentials and subscription tokens use the authentication backend. Explicit configuration, model profiles, and request overrides remain authoritative over remembered defaults.

Healthy returning launches skip setup. Missing selected credentials, disabled providers, and locally known unsupported models produce targeted connection guidance rather than silently choosing another account. Detection and local catalog checks are not proof of remote authentication or credit. Providers without a bundled offline recommendation require explicit model selection in Advanced setup.

Advanced setup remains available through `bcode --onboard` or `bcode onboard`. Permissions, optional session imports, and plugin customization are not prerequisites for the first prompt. Automation and noninteractive invocations do not open the quick chooser.

Running `bcode` after setup opens the TUI. The client starts a matching local daemon automatically when needed; normal use does not require a separate server command.

## Choose a provider

The bundled distribution includes two provider integrations:

* **OpenAI-compatible:** OpenAI API keys, ChatGPT browser or device-code login, xAI API keys, and configurable compatible endpoints.
* **Amazon Bedrock:** models available through the Bedrock `ConverseStream` API and the active AWS credential chain, plus Anthropic and `OpenAI` models served through the Bedrock Mantle endpoint. See [Bedrock-hosted OpenAI models](bedrock-openai-models.md) for the GPT-5.6 tier and other `OpenAI` models.

For OpenAI or ChatGPT authentication, the canonical CLI flow is:

```sh
bcode auth providers
bcode auth login openai
bcode auth status openai
```

Use `--method api_key`, `--method chatgpt`, or `--method device` to choose a specific registered OpenAI authentication path. Provider secrets are enrolled through Bcode's auth flow rather than written directly into ordinary configuration.

Environment-based configuration is also supported, including `BCODE_OPENAI_API_KEY`, `OPENAI_API_KEY`, `BCODE_OPENAI_MODEL`, and the corresponding Bedrock and xAI variables. Run `bcode model list` to inspect models visible through the configured provider.

## Start working

Open Bcode in a repository:

```sh
cd path/to/repository
/path/to/bcode
```

Useful entry points:

* Run `bcode -n` to create a new session immediately.
* Run `bcode -n --worktree my-task` to start a session in a new Git worktree.
* Press `Ctrl-F` in the TUI to open the command palette.
* Use `/plan` for read-oriented analysis and `/build` when implementation is allowed.
* Use `/sessions` to switch sessions and `/compact` to compact long context.
* Press `Esc` to interrupt active work.

Bcode asks before sensitive operations according to the active agent's policy. Permission dialogs show the normalized operation and support one-time, batch, and remembered decisions when applicable. See [Permissions](permissions.md) for configuration and precedence.

## Full distribution build

The complete release feature composition adds bundled OCR runtimes, Mermaid rendering, and the optional web renderer:

```sh
cargo build --locked --release -p bcode --features distribution --bin bcode
```

Release automation and supported artifact targets are documented in [Release builds](release-builds.md). Public artifacts have not been published yet.

## Configuration

Bcode loads global and repository-local `bcode.toml` files. Common locations include:

```text
~/.config/bcode/bcode.toml
<repository>/bcode.toml
<repository>/.bcode/bcode.toml
```

The documentation site generates its complete configuration and CLI references from the Rust schema and command tree. Until that site is publicly deployed, generate it locally with:

```sh
cargo run --release -p bcode_docs_site --bin bcode-docs-site -- gen --output dist
```

Focused guides:

* [TUI keybindings](tui-keybindings.md)
* [Permissions](permissions.md)
* [Worktrees](worktrees.md)
* [Skills](skills.md)
* [Session imports](session-import-plugins.md)
* [Plugins](plugins.md)
