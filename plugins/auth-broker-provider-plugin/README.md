# Scoped auth broker provider

`bcode.auth-broker-provider` implements actual normalized start/poll/cancel/finish HTTP
operations over the admitted broker gateway. It has native dynamic and `static-bundled`
entry points. Workspace registration and distribution bundling are required by the host.

Configure an explicit model profile:

```toml
[model.profiles.github-broker]
provider_plugin_id = "bcode.auth-broker-provider"
model_id = "broker-scoped"
[model.profiles.github-broker.settings]
broker_url = "https://broker.example"
```

Supply `BCODE_AUTH_BROKER_TOKEN` (opaque short-lived lease) and
`BCODE_AUTH_BROKER_EXPIRES_AT` (UNIX seconds) in the private invocation/process environment.
No subscription tokens, upstream model IDs, auth profile, API key or pool are accepted.
The remote model is selected by trusted broker admission, not by callers.

The client supports normalized text, bounded tool definitions, tool-choice/schema policy,
assistant tool calls and text-only tool results. Broker admission owns the upstream model;
the gateway independently negotiates requested tool capabilities and rejects unsupported
models/policy before start. Tool execution remains on the local Bcode client under its
ordinary permissions, never on the broker. Images, reasoning, provider extensions,
structured output and model-managed context remain unsupported. This is source-level
contract integration, not proof of a live coding-agent/provider deployment.

HTTPS origins only; no redirects or URL credentials. Requests/responses are bounded to
1 MiB; connection timeout is 10 seconds, total exchange timeout 30 seconds, and local turn
retention is limited to 64. There is no automatic start retry after ambiguous transport
failure. Poll validates cursor order and accepts only normalized text/tool, usage and terminal events;
provider-private/error payloads are not surfaced. Cancel/finish target the returned opaque
turn identity under the same lease. Finish rejection leaves local state for explicit retry.
Lease expiry rejects further requests; the broker owns expiry cancellation/reaping.
Poll failures attempt explicit finish; unverified cleanup is surfaced and retains local
identity for retry. State locks are released before network operations; reservations bound
concurrent starts and per-turn claims serialize lifecycle/cursor changes.
Errors and `Debug` never expose bearer tokens, response bodies or upstream auth.

Offline tests cover HTTPS/lease rejection, secret-safe Debug, route confinement, cursor
validation, private event rejection, text/tool delivery, tool-result continuation and uncertain cleanup retention. Offline tests include a bounded loopback HTTP mock exercising start -> tool poll ->
string tool-result continuation -> cancel -> rejected finish -> verified explicit retry.
Every captured start body is decoded and semantically validated by the shared gateway
contract. A regression test rejects object-valued tool output, and a malformed poll test
requires cleanup uncertainty to surface while preserving retry identity/cursor. Loopback
HTTP is constructed only inside the test module; production configuration still requires HTTPS.
These tests do not establish live provider support, deployment, external ingress or
GitHub workflow acceptance.
