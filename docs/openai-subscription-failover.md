# OpenAI subscription failover

Bcode can use multiple `ChatGPT`/Codex subscription logins for the same OpenAI-compatible provider. When one subscription reports quota or rate-limit exhaustion, Bcode tries the next configured subscription.

## Friendly setup

Log in normally for the first subscription:

```sh
bcode login openai
```

Add another subscription:

```sh
bcode login openai --add-subscription
```

`--add-subscription` implies `ChatGPT` OAuth mode. API-key pools are not supported yet, so `--api-key` and `--base-url` are rejected with `--add-subscription`.

Inspect the declared pool and any runtime cooldown state:

```sh
bcode auth pool status openai
```

Reset cooldown state if you know an account is usable again:

```sh
bcode auth pool reset-cooldown openai openai
# or reset all profiles in the pool
bcode auth pool reset-cooldown openai
```

List profiles:

```sh
bcode auth profile list
bcode auth profile show openai-2
bcode auth pool profiles openai
```

Refresh an existing secondary subscription token by passing its profile name:

```sh
bcode login openai --add-subscription --profile openai-2
```

Without `--profile`, `--add-subscription` registers the next new runtime profile, such as `openai-3`.

## Declarative config

The CLI writes ordinary declarative config. You can also author it directly:

```toml
[auth.profiles.openai]
backend = "sshenv"
scheme = "chatgpt"

[auth.profiles.openai.settings]
provider = "openai"
profile = "openai"
vault = "~/.local/share/bcode/auth.sshenv"
mode = "chatgpt"

[auth.profiles.openai-2]
backend = "sshenv"
scheme = "chatgpt"

[auth.profiles.openai-2.settings]
provider = "openai"
profile = "openai-2"
vault = "~/.local/share/bcode/auth.sshenv"
mode = "chatgpt"

[auth.pools.openai]
provider_plugin_id = "bcode.openai-compatible"
strategy = "failover"
profiles = ["openai", "openai-2"]

[model.profiles.openai]
provider_plugin_id = "bcode.openai-compatible"
model_id = "gpt-5.5"
auth_pool = "openai"
```

Config declares the desired subscription candidates. Runtime quota/cooldown observations are stored under Bcode's state directory and do not mutate declarative config.

## Routing strategies

Auth pools default to failover routing, which tries profiles in configured order and only moves to the next profile when the active subscription reports quota or rate-limit exhaustion.

Use `strategy = "round_robin"` to rotate the host-selected profile for each request:

```toml
[auth.pools.openai]
provider_plugin_id = "bcode.openai-compatible"
strategy = "round_robin"
profiles = ["openai", "openai-2", "openai-3"]
```

Round-robin selection is recorded after successful requests, so the next request starts with the following profile. This intentionally starts provider reset timers across subscriptions earlier instead of draining one subscription before touching the next.

You can also enable priming as a gateway before the configured strategy. Priming selects unprimed profiles before normal failover or round-robin routing, then returns to the configured strategy after those profiles have succeeded once:

```toml
[auth.pools.openai]
provider_plugin_id = "bcode.openai-compatible"
strategy = "round_robin"
profiles = ["openai", "openai-2", "openai-3"]

[auth.pools.openai.priming]
enabled = true
include_primary = false
reprime_after = "7d"
```

`include_primary = false` primes only secondary subscriptions. `reprime_after` treats old priming successes as stale after the configured duration (`s`, `m`, `h`, or `d`).

## Provider-native reuse safety

Bcode scopes provider-native conversation reuse state by auth profile. The host selects the subscription before planning reuse, so native provider response IDs and encrypted reasoning state are reused only with the same auth profile. If OpenAI quota handling falls back to a different subscription after the request was built, the plugin suppresses provider-native reuse state for that fallback request.

## Failover behavior

Bcode tries profiles in pool order. On quota-like OpenAI errors, it marks the profile on cooldown and retries the next available profile. If all profiles are unavailable, Bcode reports a friendly error suggesting another login or waiting for reset.
