# Plugins

Bcode uses plugins for domain behavior rather than treating extensibility as a thin callback layer. Model providers, tools, authentication methods, commands, skills, workflows, session importers, visual adapters, and complete TUI surfaces can all be plugin-owned.

The initial plugin runtime loads native Rust dynamic libraries. Plugins are trusted native code, not sandboxed extensions.

## Bundled and discovered plugins

Release builds can statically bundle plugins into the Bcode executable. Bundling makes a plugin available; activation policy determines whether it is loaded. Bundled defaults can be disabled, and available plugins can be enabled explicitly.

Bcode also discovers manifest-driven native plugins from:

```text
<current-directory>/.bcode/plugins
$XDG_CONFIG_HOME/bcode/plugins
~/.config/bcode/plugins
<directory-containing-bcode>/plugins
```

A discovery root may contain a plugin manifest directly or plugin subdirectories containing `bcode-plugin.toml`.

Inspect available plugins and services with:

```sh
bcode plugin list
bcode plugin services
bcode plugin check
```

## Selection

Plugin loading and model-callable tool exposure are separate choices:

```toml
[plugins]
default = "bundled" # bundled | none | all
enabled = ["bcode.vim-edit"]
disabled = ["bcode.blims"]

[tools]
default = "agent" # agent | none | all
enabled = []
disabled = ["vim_edit.apply"]
```

A plugin may provide commands or UI without exposing a model-callable tool. Agent profiles apply an additional tool policy after plugin selection.

## Manifest and service boundaries

A plugin manifest declares stable identity, runtime ABI information, concurrency policy, and versioned contributions. Depending on its domain, it may declare:

* typed service interfaces and operations;
* model-callable tools;
* authentication providers and enrollment methods;
* command-palette and slash-command contributions;
* workflow blocks and templates;
* event subscriptions;
* structured visual adapters;
* renderer-native TUI surfaces.

Cross-boundary requests and responses use Bcode-owned serializable contracts. Product hosts own discovery, loading, routing, lifecycle, and contract enforcement; the plugin retains its domain rules.

## Build the example plugin

The [`hello-plugin`](../examples/hello-plugin) example is a native dynamic library with a manifest, service, event subscriptions, authentication contribution, and serialized TUI visual adapter. It is primarily a smoke-test fixture, but it demonstrates the complete loading boundary.

## Serialized visual presentation roles

Serialized TUI visual responses are versioned independently from tool/artifact schemas. Contract version 1 remains readable and supports concrete terminal foreground colors. Contract version 2 adds renderer-neutral `role` values for text, muted, accent, info, success, warning, error, and diff states.

When both `role` and `foreground` are present, the active renderer theme resolves the semantic role and takes precedence. The concrete color remains a compatibility fallback when no renderer theme is available. Unknown future response versions fail closed and fall through to the normal native or generic presentation path rather than being guessed. A response declaring semantic roles with version 1 is rejected.

The host includes the resolved theme fingerprint in serialized visual context and dynamic cache identity. This metadata may invalidate derived presentation only; plugins must not use it for routing, authorization, dispatch, or persisted outcomes.


```sh
cargo build -p bcode_hello_plugin
```

The built library name is platform-specific:

```text
target/debug/libbcode_hello_plugin.dylib  # macOS
target/debug/libbcode_hello_plugin.so     # Linux
target/debug/bcode_hello_plugin.dll       # Windows
```

Place the library and an adjusted `bcode-plugin.toml` together under a discovery root, then run `bcode plugin check` before enabling it. The repository's [`smoke-native-plugin.sh`](../scripts/smoke-native-plugin.sh) demonstrates discovery, loading, service invocation, daemon routing, and event delivery.

## Plugin-owned authentication

Enabled plugins register their authentication providers and methods. The host owns hidden prompting, secure credential custody, ownership checks, and invocation-scoped delivery; plugins own provider identity, credential declarations, OAuth/device protocols, refresh, verification, and revocation.

```sh
bcode auth providers
bcode auth login <provider> [--method <method>]
bcode auth status <provider>
bcode auth logout <provider>
```

See [Dynamic plugin authentication](dynamic-plugin-authentication.md).

## Plugin-owned session fork and clone

The bundled `bcode.session-derivation` plugin exclusively contributes `/fork`, `/clone`, and their
palette entries. It owns prompt selection, default names, cutoffs, user-facing status, and
post-success navigation. The host exposes only versioned generic command context, a typed
`bcode.session-derivation/v1` application service, generic plugin surfaces, and portable command
effects.

Fork prompt discovery is generation-pinned and bounded. Renderer-neutral Markdown presentation is
always returned as a fallback; the native TUI adapter adds direct selection. After selection, the
plugin separately retrieves the complete canonical user message so truncated preview text never
becomes authoritative draft state. Disabling the plugin removes these product workflows without
affecting session storage, normal session use, or workflow-owned fixed-generation derivation.

## Workflows and presentation

Plugins can contribute typed workflow blocks and immutable templates without owning the generic scheduler or durable workflow store. They can also publish structured presentation payloads with renderer-specific adapters and a generic fallback.

See:

* [Workflow plugins and templates](workflow-plugins-and-templates.md)
* [Plugin-authored durable workflows](plugin-workflows.md)
* [Plugin presentation updates](plugin-live-progress.md)
* [Tool runtime ownership](tool-runtime-contract-ownership.md)
