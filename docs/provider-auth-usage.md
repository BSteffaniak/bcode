# Provider auth usage capability

Bcode exposes provider auth quota/usage through the model-provider service operation `OP_AUTH_USAGE`.
Providers that can inspect subscription, account, or credential usage should implement this operation and return normalized meters and windows. The CLI, cache, status display, and routing/priming policy can then consume the data without provider-specific code.

## Turn authentication preparation

Session requests and CLI model verification use `bcode_provider_auth::auth_pool_routing`
for the same pre-turn preparation: refresh the runtime pool preference, probe usage needed for
priming on a best-effort basis, then apply current cooldown/strategy/priming selection. The selected
profile, auth material, environment, and selection reason move together. Candidates remain intact
for provider-owned fallback; preparation neither retries turns nor interprets provider auth errors.
CLI verification applies this at each typed `BlockingModelProviderInvoker::start_turn`, including
all prompt-cache scenario rounds, rather than relying only on the configured profile.

## Using credentials from other coding agents

Bcode can use credentials that another coding agent already keeps on the machine, in place and
with explicit consent. Nothing is imported into sshenv, copied, refreshed, or rewritten.

* **Declarations.** Provider plugins declare `AuthExternalSource`s on their
  `AuthProviderContribution`: the owning application, the method the document satisfies, a
  home-relative path with an optional root override variable (for example `CODEX_HOME` or
  `XDG_DATA_HOME`), an optional discriminator, pointers that must be absent, the credential
  pointers, an optional account pointer, and a literal-value policy. The bundled OpenAI-compatible
  provider declares Codex (API-key mode and ChatGPT sign-in), OpenCode API keys, and Pi literal
  API keys.
* **Discovery** reports document presence only and never parses a document.
* **Approval** (`bcode auth connect`, or the first-run Setup picker) reads the document once to
  reject unsupported formats immediately, and records a truncated SHA-256 of the declared account
  identity. Consent covers every local Bcode application using the same state location; caller
  labels are not trusted as application identity. Approvals live in
  `<state>/auth/external-connections.json`. `bcode auth connections` lists them;
  `bcode auth disconnect` removes one without signing out the owning application.
* **Binding identity** is a digest of the plugin, provider, method, source ID, and resolved
  document path. Cosmetic declaration edits and credential rotation keep it; a different source
  ID or document location produce a new binding.
* **Selection.** Choosing a model saves `external_auth_binding` in the startup selection, which
  becomes `model.external_auth_binding` and the typed `ProviderRequestContext::external_auth_binding`.
  Clients and the daemon carry only this credential-free recipe.
* **Dispatch.** The daemon (or a direct CLI probe) calls
  `ExternalAuthStore::resolve_dispatch`, which rejects recipes that also carry a profile, pool,
  request overrides, environment values, or materialized credentials; requires the request's
  model to be one the provider recommends for the approved method, with matching routing settings:
  every key the provider declares in `routing_settings` (including aliases such as
  `openai.base_url`) and every key the recommendation sets must have the same value, so
  configuration cannot redirect the credential, while unrelated settings are free (a provider that
  declares none requires an exact match); then reads the document under a shared
  lock (revocation takes it exclusively), rejects an account change with `AccountChanged`, and
  delivers the values for that request only. Requests carrying a binding get no credential custody
  bridge, so providers cannot write credentials back. There is never a fallback to another account
  or billing route.
* **Status.** `bcode auth connections` and the `/model` picker show each connection as ready,
  expired, signed in to a different account, not found, or no longer offered, with the one action
  that fixes it. Sources may declare an expiry (for example the JWT `exp` of Codex's access token),
  checked locally so an expired sign-in is reported before a request (`Expired`).
* **Choosing a connection in a session.** The `/model` picker lists, after the provider's own
  models, one row per model each approved connection can use ("model · via Codex"). Selecting a
  ready row sets the session's model and connection together, applied from the next turn like a
  model change. Selecting a row whose source changed accounts approves it again and reloads the
  picker. Discovered credentials that are not connected yet appear as "Connect …" rows; pressing
  Enter asks for confirmation and a second Enter approves them. When a chat opens, Bcode mentions
  newly found, unconnected credentials once (recorded in `external-connection-notices.toml` in the
  state directory, binding digests only), honoring the discovery opt-out. The choice is recorded on
  the session's `model_changed` event and survives daemon
  restarts; if the connection later becomes unusable, the session keeps it and requests fail with
  the reason rather than switching to another account.
* **ChatGPT tokens** from Codex are delivered without the refresh token. The source declares the
  access token's `exp` as its expiry; once it passes, the request fails before reaching the
  provider, asking the user to open Codex to renew the sign-in. Codex installs that
  keep credentials in the OS keyring instead of `auth.json` are not discovered.
* **Usage.** `bcode auth connections --usage` shows each ready connection's usage windows, such as
  ChatGPT limits for a Codex sign-in, by resolving the connection in place exactly as dispatch does.
* **Unsupported operations.** `bcode model capabilities` and image-upload verification route
  through generic daemon plugin calls, not dispatch, and reject external connections explicitly.
  `bcode auth usage` reports auth pools and is unaffected by an external default selection.

The first-run Setup picker groups options by agent: for Codex, "Run Codex" (the native harness,
using Codex's own sign-in and settings) sits next to "Use Codex credentials in Bcode" (Bcode's agent
reading the approved document in place). Choosing a connection takes one confirmation: Bcode saves
the provider's recommended model with it and starts a session; `/model` changes the model later.
Native installations that are already signed in go straight to choosing a model; otherwise setup
offers the tool's own sign-in first. Native credentials stay with the native agent. Bcode does not
set `CLAUDE_CONFIG_DIR` for the default `~/.claude` namespace, because Claude Code selects a
different OS credential-store entry when the variable is set, even to the default path.

## Operation

Request type: `bcode_model::AuthUsageRequest`

* `provider_context` identifies the auth material/profile to inspect.
* `meter_ids` optionally narrows the meters the caller is interested in. Providers may ignore this hint.

Response type: `bcode_model::AuthUsageResponse`

* `supported` is `true` when the provider/auth mode supports usage discovery.
* `degraded_reason` explains unsupported or incomplete data.
* `debug` contains provider-owned diagnostic strings. Do not include secrets.
* `capabilities.features` lists capability flags such as `refresh`, `window_reset`, `used_percent`, `absolute_amounts`, `priming`, and `reset_credits`.
* `reset_credits` optionally contains a summary of banked provider reset credits when the usage endpoint returns one.
* `meters` contains normalized provider usage data.
* `priming_windows` optionally identifies the provider-selected applicable windows for automatic priming. Explicit configured requirements take precedence. `None` means no recommendation; an empty map must not expand to unrelated usage meters.

OpenAI recommends only the windows returned for the account's main Codex meter, not additional model-specific quotas. Weekly-only accounts do not require a secondary window. Refresh status after upgrading to replace older cached assumptions. Zero rounded usage remains unconfirmed; local request success does not fabricate provider usage.

Explicit OpenAI priming consumes the model stream through successful terminal completion before refreshing usage. HTTP acceptance alone, stream errors, premature EOF, and truncated generation do not establish successful verification. Priming does not automatically change the pool's `include_primary` policy or saved preference.

## Meter and window semantics

Each `AuthUsageMeterSnapshot` is one stable provider-owned quota bucket.

* `meter_id` must be stable across refreshes. Prefer the provider's canonical feature, quota bucket, organization, or account id.
* `meter_name` is optional display text.
* `windows` are the known quota windows for that meter.

Each `AuthUsageWindowSnapshot` is one stable window inside a meter.

* `window_id` must be stable within the meter, such as `primary`, `secondary`, `hourly`, `daily`, or `monthly`.
* `window_duration_secs` should be set when known.
* `resets_at_unix` should be set when known.
* `used_percent` should preserve provider-rounded percent values when available.
* `used_amount` and `limit_amount` should be set when providers expose absolute quota units.
* `observed_at_unix` is the local observation timestamp.
* `source` identifies the provider API or data source.

## Priming

`OP_AUTH_PRIME` is optional and separate from usage discovery. Providers that support usage inspection but cannot intentionally touch usage windows should return `supported: true` from `OP_AUTH_USAGE` and `Unsupported` from `OP_AUTH_PRIME`.

Routing and status code should treat usage discovery as reporting provider state, while priming policy decides whether a synthetic request is useful for a specific provider/pool.

## Banked rate-limit resets

Some providers expose banked rate-limit reset credits. Bcode exposes these through separate read and consume operations so spending a reset is always explicit.

Read operation: `OP_AUTH_RESET_CREDITS`

* Request type: `bcode_model::AuthResetCreditsRequest`
* Response type: `bcode_model::AuthResetCreditsResponse`
* Providers should return `supported: false` rather than failing when the auth mode cannot list reset credits.

Consume operation: `OP_AUTH_RESET_CREDIT_CONSUME`

* Request type: `bcode_model::AuthResetCreditConsumeRequest`
* Response type: `bcode_model::AuthResetCreditConsumeResponse`
* `redeem_request_id` is an idempotency key for one logical reset attempt.
* `credit_id` optionally selects a provider credit; when omitted, providers may choose the next available credit.
* Normal routing, failover, status, and priming paths must not consume reset credits automatically.

OpenAI/Codex-compatible providers use the ChatGPT backend reset-credit endpoints when ChatGPT Codex auth is active.

## Example

```rust
Ok(bcode_model::AuthUsageResponse {
    supported: true,
    degraded_reason: None,
    debug: BTreeMap::new(),
    capabilities: bcode_model::AuthUsageCapabilities {
        features: BTreeSet::from([
            bcode_model::AuthUsageCapability::Refresh,
            bcode_model::AuthUsageCapability::WindowReset,
            bcode_model::AuthUsageCapability::UsedPercent,
        ]),
    },
    meters: vec![bcode_model::AuthUsageMeterSnapshot {
        meter_id: "requests".to_string(),
        meter_name: Some("Request quota".to_string()),
        windows: vec![bcode_model::AuthUsageWindowSnapshot {
            window_id: "daily".to_string(),
            window_duration_secs: Some(86_400),
            resets_at_unix: Some(reset_timestamp),
            used_percent: Some(42),
            used_amount: None,
            limit_amount: None,
            observed_at_unix,
            source: Some("provider_usage_api".to_string()),
        }],
    }],
    reset_credits: Some(bcode_model::AuthResetCreditsSummary {
        available_count: 1,
    }),
})
```
