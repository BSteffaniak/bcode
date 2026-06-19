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

## Failover behavior

Bcode tries profiles in pool order. On quota-like OpenAI errors, it marks the profile on cooldown and retries the next available profile. If all profiles are unavailable, Bcode reports a friendly error suggesting another login or waiting for reset.
