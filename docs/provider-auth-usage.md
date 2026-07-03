# Provider auth usage capability

Bcode exposes provider auth quota/usage through the model-provider service operation `OP_AUTH_USAGE`.
Providers that can inspect subscription, account, or credential usage should implement this operation and return normalized meters and windows. The CLI, cache, status display, and routing/priming policy can then consume the data without provider-specific code.

## Operation

Request type: `bcode_model::AuthUsageRequest`

* `provider_context` identifies the auth material/profile to inspect.
* `meter_ids` optionally narrows the meters the caller is interested in. Providers may ignore this hint.

Response type: `bcode_model::AuthUsageResponse`

* `supported` is `true` when the provider/auth mode supports usage discovery.
* `degraded_reason` explains unsupported or incomplete data.
* `debug` contains provider-owned diagnostic strings. Do not include secrets.
* `capabilities.features` lists capability flags such as `refresh`, `window_reset`, `used_percent`, `absolute_amounts`, and `priming`.
* `meters` contains normalized provider usage data.

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
})
```
