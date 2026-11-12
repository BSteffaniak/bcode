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

## External connections without credential import

`bcode auth connect` offers supported external API-key sources for explicit approval.
`bcode auth connections` lists approvals; `bcode auth disconnect` revokes one.
Approval stores a source binding and consent, not a credential copy in sshenv.
The source application remains responsible for its credential file. Normal model
execution acquires the selected credential at daemon dispatch, so source rotation
and revocation are observed on subsequent dispatches. Explicit external selection
fails closed rather than falling back to another account or billing route.

Discovery inspects bounded source metadata, not credential contents. Presence is
not remote validity. Acquisition validates the provider-declared file format after
consent. Supported static-key declarations cover Codex API-key mode, OpenCode API
entries, and Pi literal API-key entries. Dynamic Pi commands, environment references,
OAuth entries, and mixed/unsupported Codex authentication documents are rejected.
Automatic discovery respects the onboarding discovery opt-out. Headless operation
never implicitly approves a source.

Consent currently covers local Bcode applications. Application-specific approval
is rejected until the application boundary supplies an authenticated application
identity; caller-provided labels are not authority. Explicit provider/model/profile
selection remains authoritative. Import remains a separate, optional operation.

### Native subscriptions and harnesses

Native Claude Code and Codex accounts remain native-managed; discovery is not
permission to extract or refresh their OAuth tokens. Enabled plugins expose bounded
passive installation discovery and consent-gated preparation through
`bcode.harness.native-connection/v1`. `auth native-connect`, `auth native-connections`,
and `auth native-disconnect` use the application boundary for approval, inspection,
and revocation. Startup also offers native setup through the existing backend picker.
Approval records the exact installation/namespace binding in durable Bcode user state;
no native credentials, declarative configuration, or plugin cache become consent authority.

Preparation publication rechecks the consent revision. Approved prepared records are
merged into the managed harness catalog, and native plugins resolve only their own
host-published instance through an invocation-bound lookup. Revocation blocks new use
without logging out the source account or claiming ongoing work was cancelled.
Provider-independent bootstrap does not require a model provider or vault login.

Claude has a narrowly pinned `2.1.308` wrapper/payload profile for isolated optional
settings, native-account status, and stream initialization. Preparation requires a
logged-in native Claude account and rechecks artifact/namespace identity. This profile
retains **unknown settlement** and does not claim successful generation, tool execution,
authentication mutation, or durable recovery. A bounded installed-account generation
attempt returned an error with limit/reset indicators; it did not establish live success.
Codex preparation remains `CompatibilityUnavailable`: empty CLI table overlays merge
rather than remove inherited MCP/provider settings, and no verified public settings-layer
exclusion retaining native credentials has been established. See the plugin READMEs for
exact compatibility scope and the required upstream capability.

Do not auto-enable verified flags or describe installation/login status as successful
execution. Existing explicitly configured harnesses retain their admission requirements.
Synthetic tests cover canonical admission, approval denial, interrupt retention, and
stale-owner fencing—not installed protocol settlement or successful restart recovery.

Native application consent and publication filesystem operations run on blocking tasks;
the global catalog lookup also moves durable reads off executor threads. Synchronous
instance-bound lookup remains on the native plugin's blocking invocation thread. Dropping
an application request does not prove an already-admitted blocking mutation had no effects;
revision/mutation checks remain the authority for retries.

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
