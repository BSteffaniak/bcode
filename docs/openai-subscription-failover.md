# OpenAI subscription failover

Bcode can use multiple `ChatGPT`/Codex subscription logins for the same OpenAI-compatible provider. When one subscription reports quota or rate-limit exhaustion, Bcode tries the next configured subscription.

## Canonical setup

Enroll the primary subscription through OpenAI's plugin-registered browser flow:

```sh
bcode auth login openai --method chatgpt
```

Use `--method device` for device-code enrollment. Add a named secondary subscription to the runtime failover pool:

```sh
bcode auth login openai --method chatgpt --profile openai-2 --pool openai
```

Pool enrollment does not replace the primary provider binding. API-key pools are not supported; provider-specific base URLs and models remain model/provider configuration rather than generic auth enrollment.

### Automatic subscription pools

For one pool policy shared by multiple model presets, see
[Shared authentication pool policy](shared-auth-pools.md). The explicit
`auth_pool_config` reference avoids copying this section into every preset.

With a provider that advertises compatible subscription methods, login without `--pool` joins or creates an explicitly managed default pool. OpenAI browser and device-code logins share the `subscription` group; API keys do not participate. For example:

```sh
bcode auth login openai --method chatgpt --profile openai-2
```

Use `--standalone` to register an account without adding pool membership. `--pool <name>` selects an explicit destination instead. Login reports whether membership was added and whether the destination is automatic or manual. A storage/publication failure is reported separately from successful authentication; do not assume repeating OAuth repairs metadata.

Existing unmarked pools stay manually managed, even when only one pool exists or its name is `openai`. To explicitly opt an existing pool into compatible automatic enrollment:

```toml
[auth.pools.openai]
provider_plugin_id = "bcode.openai-compatible"
automatic_provider = "openai"
automatic_group = "subscription"
profiles = ["openai"]
```

The provider and compatibility group are identities, not display names. Unknown, incompatible, ambiguous, or conflicting destinations fail closed rather than adopting a similarly named pool. Enrollment never rewrites declarative configuration. A fresh automatic OpenAI pool is named `openai-subscription-default`; name collisions are not adopted.

Native daemon-owned default routing can discover new managed members before the next turn, without restarting. Explicit account-only selection and unmarked manual pools retain their previous behavior. Caller-supplied runtime contexts, external resolvers, and retained credential custody remain snapshots: pool membership does not grant new credential-acquisition authority. A turn freezes its membership across retries and compaction. Existing member credentials are not refreshed by membership discovery; removed/readded profile names require a fresh native snapshot. Named configuration contexts remain declared-only and never inherit global runtime accounts.


The deprecated `bcode login openai` and `bcode login openai --add-subscription` commands remain temporary compatibility shims. New automation and documentation should use `bcode auth login`.

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
bcode auth login openai --method chatgpt --profile openai-2 --pool openai
```

With a new profile name, canonical `--pool openai` enrollment registers another runtime profile, such as `openai-3`; passing an existing profile refreshes that profile in place.

Inspect banked reset credits:

```sh
bcode auth resets status openai
bcode auth resets status openai --profile openai-2
bcode auth resets status openai --profile openai-2 --verbose
```

Spend one banked reset credit explicitly:

```sh
bcode auth resets use openai --profile openai-2
# optionally target a specific provider credit id
bcode auth resets use openai --profile openai-2 --credit credit-123
```

`use` prompts for confirmation unless `--yes` is passed. To inspect what would be sent without calling the consume endpoint, use `--dry-run`.

## Declarative config

The CLI writes ordinary declarative config. You can also author it directly:

```toml
[auth.profiles.openai]
backend = "sshenv"
provider_id = "openai"
owner_plugin_id = "bcode.openai-compatible"
scheme = "chatgpt"

[auth.profiles.openai.settings]
provider = "openai"
profile = "openai"
vault = "~/.local/share/bcode/auth.sshenv"
mode = "chatgpt"

[auth.profiles.openai-2]
backend = "sshenv"
provider_id = "openai"
owner_plugin_id = "bcode.openai-compatible"
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

Pool selection is explicit: set `auth_pool` on the active model selection to opt into
multi-account routing. Selecting only `auth_profile` uses that account and does not infer
an `openai` pool from the authentication scheme. Pool and profile names are user-defined;
`openai` is an example, not a reserved pool name.

## Preferred profile and effective order

Auth pools support any number of profiles. The first profile in effective order is the preferred
profile for failover and for `priming.include_primary` semantics. Declare a default without changing
the underlying profile list:

```toml
[auth.pools.openai]
provider_plugin_id = "bcode.openai-compatible"
strategy = "failover"
profiles = ["openai", "openai-2", "openai-3"]
preferred_profile = "openai-2"
```

Promote any member interactively from the CLI:

```sh
bcode auth pool promote openai openai-3
bcode auth pool clear-preference openai
```

The interactive choice is stored in user state and wins over `preferred_profile`; it never rewrites
declarative configuration. Promotion moves only the selected profile to the front and preserves the
relative order of every other profile. In the TUI, `/auth-pool` and `/subscriptions` open the same
generic profile picker. Pool status reports the effective order and preference source.

## Explicit reconciliation of split automatic pools

Pool names are identities, not aliases. Selecting `openai` does not inherit accounts from
`openai-subscription-default`. Conflicting automatic pools for the same provider/plugin/group
surface degraded status and block strict selection and managed-default routing.

```sh
bcode auth pool reconcile --source openai-subscription-default --destination openai
bcode auth pool reconcile --source openai-subscription-default --destination openai --apply
```

The default is a read-only preview. The destination must be a declared automatic pool with the
same verified owner and compatibility group. The source must be runtime-only and no loaded model
selection may reference it. Conflicting credential destinations, seal policy, preferences, or
observations reject the operation. Credentials and provider bindings are unchanged. References
from other configuration files or clients must be updated explicitly.

Apply with pool activity quiescent, especially while older daemon artifacts are running: old
artifacts do not participate in observation locking. Reconciliation locks and revision-checks the
registry, retains a registry backup, atomically copies observations while retaining their source,
then atomically publishes destination membership and removes the redundant runtime pool. New
observation writers share a filesystem lock. Failed publication leaves source observations intact
for retry. Unknown registry/observation fields are not discarded. Restart affected clients to resolve
new membership; do not reset credentials or copy tokens between machines. Membership remains
machine-local even when declarative Nix policy is shared. Priming policy is independent of whether
pool routing is enabled.

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
