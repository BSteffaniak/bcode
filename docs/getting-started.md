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
cargo build --release -p bcode \
  --no-default-features \
  --features app,static-bundled-plugins \
  --bin bcode
```

Run it:

```sh
./target/release/bcode
```

On Windows:

```powershell
.\target\release\bcode.exe
```

The first normal interactive launch opens Bcode's setup flow. It detects existing configuration and environment hints, then walks through provider, model, authentication, permissions, optional session imports, and plugin choices.

Running `bcode` after setup opens the TUI. The client starts a matching local daemon automatically when needed; normal use does not require a separate server command.

## Choose a provider

The bundled distribution includes two provider integrations:

* **OpenAI-compatible:** OpenAI API keys, ChatGPT browser or device-code login, xAI API keys, and configurable compatible endpoints.
* **Amazon Bedrock:** models available through the Bedrock `ConverseStream` API and the active AWS credential chain.

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
cargo build --release -p bcode --features distribution --bin bcode
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
