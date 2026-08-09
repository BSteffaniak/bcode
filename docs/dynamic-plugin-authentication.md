# Dynamic Plugin Authentication Architecture

Bcode discovers authentication providers from enabled plugins. Generic host code owns secure enrollment and lifecycle plumbing; plugins own provider identity, credential requirements, interactive protocol behavior, and provider-specific interpretation.

## Ownership and dependency direction

### `bcode_provider_auth_models`

Owns portable, versioned registration and interactive-flow contracts:

* `AuthProviderContribution` and provider-local method IDs
* host-prompted `SecretFields` declarations
* renderer-neutral interactive requests, statuses, effects, diagnostics, and credential outputs
* validation bounds and schema versions

This leaf crate contains no vault, plugin-host, network, provider, CLI, or UI implementation.

### `bcode_plugin_sdk` and `bcode_plugin`

The SDK exposes `AuthRegistrar` and the `bcode_plugin_register_auth_providers_v1` registration hook. The plugin host attaches the registering plugin ID as canonical ownership and builds a deterministic `AuthProviderRegistry`. Duplicate provider IDs, malformed contributions, and unsupported contracts fail closed.

Only enabled, successfully activated plugins contribute providers. Disabling a bundled plugin removes its auth contribution without changing unrelated providers or core Bcode operation.

### Provider and integration plugins

Plugins own:

* provider IDs and display names
* method IDs and credential declarations
* storage-key declarations for host custody
* API endpoints, OAuth/device-code behavior, token exchange, refresh, verification, and revocation
* provider-specific diagnostics and interpretation

A plugin registers `SecretFields` when the host can prompt for bounded hidden values. It registers `Interactive` when enrollment needs provider-owned browser, device-code, polling, or token-exchange behavior.

### Host authentication services

`bcode_provider_auth` and host orchestration own:

* provider/plugin/profile ownership checks
* declarative and runtime profile resolution
* hidden prompting for secret fields
* vault initialization, reads, writes, targeted deletion, and device-seal enforcement
* normalized effect rendering and bounded flow continuation
* runtime non-secret metadata and provider bindings
* invocation-scoped credential materialization

Generic host code routes by registered provider and method identity. It must not select behavior by matching `openai`, `xai`, `exa`, or another provider ID.

## Registration and method contracts

A plugin registers during activation through `RustPlugin::register_auth_providers` or its concurrent equivalent. A contribution has a schema version, provider ID, display name, and one or more methods.

`SecretFields` declares canonical credential IDs, backend storage keys, hidden prompts, optionality, and bounded local validation. The host validates and stores submitted values; plugins never receive vault custody merely to enroll a key.

`Interactive` declares an operation and the complete set of credentials a successful flow may return. The host invokes `bcode.auth/v1`, renders normalized effects, and stores only declared terminal credentials. Undeclared credentials fail before mutation.

Interactive effects are renderer-neutral:

* open an HTTP(S) browser URL
* display a verification URL and device code
* request a non-secret choice or confirmation
* wait for a bounded interval
* display non-secret informational text

Provider-native callback payloads, token responses, and HTTP errors remain inside the plugin.

## Profile resolution and precedence

Resolution is deterministic:

1. an explicit CLI profile
2. an explicit active auth-profile environment selection
3. an owned auth profile selected by the active model/wrapper profile
4. a declarative provider binding
5. a same-named declarative profile
6. a runtime provider binding and runtime profile
7. the provider ID as the profile name for new enrollment

Declarative profiles and bindings take precedence over runtime metadata. Explicit or active profiles with mismatched provider/plugin ownership fail closed; a model-selected profile owned by an unrelated provider is ignored for the current provider.

Runtime metadata contains provider ID, owner plugin ID, backend, scheme, storage profile, vault path, credential-to-storage mapping, and device-seal selection. It never contains credential values. Pool enrollment registers a profile without replacing the provider's primary binding.

Provider-specific base URLs, model IDs, and request behavior are provider/model configuration, not generic authentication enrollment.

## Vault custody and secret delivery

Credential values are accepted only through hidden host prompts or successful terminal interactive responses. The host writes them directly to the selected `sshenv` vault profile through the ownership-checked lifecycle API.

Plaintext values must not be copied into TOML, runtime JSON, diagnostics, flow state, command arguments, snapshots, or logs. Runtime metadata stores storage keys and profile locations only. Device sealing is host-owned policy and is enforced during mutation.

At invocation time, the host resolves the selected profile and supplies only the credentials declared by the owning method. Normalized application/provider contexts receive canonical credential IDs; unrelated plugins and providers do not receive those values.

## Lifecycle and failure behavior

Canonical commands are:

```text
bcode auth providers
bcode auth login <provider> [--method <method>] [--profile <profile>]
bcode auth status <provider> [--profile <profile>]
bcode auth logout <provider> [--profile <profile>]
```

`--pool <pool>` adds a newly enrolled profile to a runtime auth pool without changing the primary provider binding. `--no-device-seal` records and applies the host-owned opt-out. Verification and revocation run only when the registered method advertises support.

Interactive flows are bounded and explicit. Pending responses must provide resumable state; terminal responses cannot reopen the flow. Malformed, oversized, unknown-version, failed, or cancelled responses terminate without storing credentials. Cancellation is sent back through the normalized flow operation when possible. Diagnostics are bounded and non-secret.

Missing plugins/providers, duplicate registrations, ownership mismatches, damaged vaults, missing profiles, and unavailable sealing backends produce actionable errors. Damaged vault state is not silently reset, repaired, or replaced during normal status, login, logout, or invocation.

## Compatibility

Legacy environment-backed profiles and conventional provider environment variables remain supported where the owning integration documents them. Deprecated top-level provider login commands may translate arguments into canonical registered methods during migration, but they are not extension points and must not retain independent credential/OAuth implementations.

OpenAI subscription pools are read compatibly from existing runtime state. New generic code does not invent OpenAI-specific defaults; plugin registration supplies method schemes and credential mappings.

Auth pools are ordered, provider-neutral collections and may contain any number of profiles. A
declarative `preferred_profile` moves one member to the front without reordering the others.
Interactive CLI/TUI promotion is persisted in non-secret user state, takes precedence over the
declarative default, and never rewrites configuration. Portable pool summaries and mutations cross
the client/server boundary; frontends do not read auth state files directly.

## Plugin author checklist

1. Depend on `bcode_provider_auth_models` and `bcode_plugin_sdk`; do not add vault dependencies for ordinary enrollment.
2. Register every provider and method during plugin activation.
3. Use stable, bounded IDs and declare every credential the host may store or deliver.
4. Keep endpoints, token exchange, refresh, and provider-native errors inside the plugin.
5. Return only normalized effects and non-secret diagnostics.
6. Validate cancellation and terminal-state behavior.
7. Test malformed responses, missing credentials, ownership mismatch, redaction, and disabled-plugin behavior.
8. Keep API keys, tokens, codes, and callback payloads out of config, state, logs, fixtures, and snapshots.

The Exa registration in `plugins/web-search-plugin` is the minimal `SecretFields` example. OpenAI browser and device methods in `plugins/openai-compatible-provider-plugin` demonstrate normalized interactive flows.
