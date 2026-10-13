# Scoped auth broker provider

## GitHub OIDC bootstrap (version one)

```sh
bcode --no-credential-discovery --json-errors auth-broker prepare \
  --directory /private/runner/scratch --broker-url https://broker.example \
  --audience bcode --timeout-secs 600
```

`static_cli_registration()` contributes this local-only CLI without a daemon.
The manifest also advertises `bcode.auth-broker-setup/v1`, operation `prepare`, with
`PrepareRequest { directory, broker_url, audience, timeout_secs }` and
`PrepareResponse { profile }`. Unknown request fields fail closed. Both entry
points share the Rust implementation. OIDC request URL/token are environment-only;
credentials are never arguments, service payloads, config values or output.

The exchange uses bounded HTTPS requests without redirects or automatic retry.
Returned lifetime cannot exceed 900 seconds; execution must fit actual remaining
lease minus ten seconds. The 600-second default requires at least 610 seconds.
Too-long budgets return `execution_budget_exceeds_lease` before model start.
The broker owns JWT verification, configured TTL and expiry reaping. There is no
renewal or effect replay.

An existing canonical absolute owner-only directory is required, outside
`GITHUB_WORKSPACE` when present. Descriptor-relative exclusive 0600 creation writes
`broker-token`, `broker-expiry`, and typed TOML `broker.toml`. Failure rollback
removes only files created by the invocation. Existing files are never overwritten.
Settings `broker_lease_file` and `broker_expiry_file` must appear together; mixing
these with context lease environment values fails closed. The Action does not
export the lease into tool subprocess environments and removes these three files
on exit. Same-user trusted tools can still read files: this is not a sandbox.
Direct mode remains separate and keeps its 86400-second maximum.

Rust behavioral tests cover endpoint/credential/response bounds, redirects, lease
budget edges, TOML roundtrip, exclusive private files and narrow collision rollback.
`.github/actions/run-bcode/test.sh` covers Action wiring and environment/cleanup
using a mock binary. These tests are not evidence of live OIDC/provider acceptance.


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
