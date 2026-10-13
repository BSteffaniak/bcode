# Auth broker (single owner, Linux)

This plugin serves a normalized text-and-tools model gateway. It does not proxy arbitrary HTTP,
store provider credentials, enroll subscriptions, or interpret provider wire formats.

## Server admission and launch

The server owns the retained operation store, binds the global `serve` invocation, generates its
correlation identity, reaps expired turns, cancels invocation-owned turns on exit/disconnect, and
awaits workers during daemon shutdown. Only the explicitly admitted plugin's `serve` operation
receives model-host routing. HTTP policy comes from trusted startup configuration, not the IPC
payload. Existing `bcode plugin call`/`invoke --daemon` boundaries keep the call open while serving.

```toml
[plugins.config."bcode.auth-broker".model_host]
allowed_plugin_id = "bcode.auth-broker"
model_profile = "broker"
supported_account_use = true
max_turns = 128
max_scope_seconds = 900

[plugins.config."bcode.auth-broker".model_host.custody]
provider_id = "openai"
directory = "/var/lib/bcode/broker-custody"
identity_file = "/run/credentials/bcode/age-identity"

[plugins.config."bcode.auth-broker".serve]
bind = "127.0.0.1:8080"
trusted_tls_ingress = false
audience = "https://broker.example/actions"
max_leases = 128
lease_seconds = 900

[[plugins.config."bcode.auth-broker".serve.allow]]
repository_id = "123456"
repository = "owner/repository"
workflow_ref = "owner/repository/.github/workflows/agent.yml@refs/heads/main"
git_ref = "refs/heads/main"
subject = "repo:owner/repository:ref:refs/heads/main"
```

Launch using the selected executable/config/state root:

```text
bcode plugin call bcode.auth-broker/v1 serve --daemon
```

Missing/malformed admission disables the gateway rather than breaking unrelated daemon startup.
The fixed model profile must name an explicit provider/model/auth profile. Native materialization
continues for non-refresh API-key use when `custody` is omitted. Subscription remote rotation
requires retained custody: begin/commit callbacks bind the actual provider owner and the same
retained source used before dispatch. No unfenced fallback is permitted.

### Supported Linux service custody

The custody directory must already be initialized as `CredentialCustodyStorage` and hold the
explicit profile with its registered method's complete credential record. Configure the auth
profile's `settings.device_seal = "off"` explicitly for this age-only service path. The identity file must
contain one age secret identity, be an absolute owner-only regular file (no final symlink), and
fit the bounded reader. Credentials must use sshenv, no auth pool, and explicit device-seal policy
`off`; existing device-sealed/native/passphrase factors fail closed. Fresh profile encryption keys
come from sshenv's cryptographic entropy source. Identity values are zeroizing host memory and
never enter wire contracts or broker-plugin config. The directory retains an exclusive owner.

**Enrollment/migration launcher gap:** normal `bcode auth login` writes a legacy vault, not this
retained custody directory. Existing authentication alone is therefore not yet the complete
connect-once path. An explicit authorized enrollment/import command must initialize retained
custody through the provider-auth lifecycle APIs; this server does not guess, migrate or create it.

The server adapter uses centralized profile/target resolution and the existing provider-owned
start bridge. Provider refresh begin/commit remains on that provider bridge; the broker never
receives subscription access/refresh tokens. The operator connects the supported account once
through the existing provider authentication enrollment, then configures the fixed model profile.
Operator responsibility for permitted use does not imply that arbitrary accounts are supported.

## Local serve payload

```json
{
  "invocation_id": "launcher-generated-correlation",
  "bind": "127.0.0.1:8080",
  "trusted_tls_ingress": false,
  "audience": "https://broker.example/actions",
  "allow": [{
    "repository_id": "123456",
    "repository": "owner/repository",
    "workflow_ref": "owner/repository/.github/workflows/agent.yml@refs/heads/main",
    "job_workflow_ref": null,
    "git_ref": "refs/heads/main",
    "subject": "repo:owner/repository:ref:refs/heads/main"
  }],
  "max_leases": 128,
  "lease_seconds": 900
}
```

Public binding requires `trusted_tls_ingress = true` and operator-provided TLS ingress. Do not
expose the plaintext listener directly. Ingress must also impose connection, header, request-rate
and idle time limits. Do not log Authorization headers, JWTs, lease exchange responses, bodies,
or provider material. There are no browser routes and no general platform/tenant management.

## Lease/run lifetime

The default lease TTL is 900 seconds; explicit values must be in `1..=900`. The host
also rejects scopes exceeding its configured maximum (at most 900 seconds). Configure
`max_scope_seconds` to match `lease_seconds`. There is no renewal, effect replay, or
credential fallback. Exchange returns `expires_at = min(exchange_time + lease_seconds,
verified GitHub JWT exp)`: JWT expiry can shorten even the default lease substantially.
The runner must reject a budget that exceeds the returned remaining lifetime minus ten
seconds **before starting model work**. A 600-second default run can therefore fail with
a short-lived JWT; an explicit 86400-second budget is unsupported in broker mode. Such
failure must not switch to API-key mode or replay tool effects. Obtain a fresh OIDC JWT
only before a new explicit run, never as automatic renewal of admitted work.

Expiry, invocation exit/disconnect and shutdown request cancellation and finish independently
of HTTP connectivity. Cancellation acknowledgement means a request was admitted, not that
remote work stopped. Finish acknowledgement requires observed successful cleanup; failed or
ambiguous cleanup remains capacity-accounted until restart.

## HTTP contract

* `POST /v1/leases`: Authorization `Bearer <GitHub Actions OIDC JWT>`. Returns
  `{ "lease": "opaque secret", "expires_at": 123 }`. Requires exact issuer, string audience,
  RS256 signature using the fixed GitHub JWKS endpoint, required time claims, and exact policy.
  Redirects/key URLs from JWTs are forbidden. Key caching/fetches and lease retention are bounded.
* Other calls use Authorization `Bearer <lease>`.
* `POST /v1/turns`: `{ "request_id": "stable-id", "messages": [{"role":"user","content":[{"type":"text","text":"hello"}]}],
  "tools": [], "tool_call_policy": {"choice":{"mode":"auto"}},
  "tool_schema_mode": null, "max_output_tokens": 1024 }` returns `{ "turn_id": "opaque-host-id" }`.
* `POST /v1/turns/{id}/poll`: `{ "after_sequence": 0 }` returns a bounded page of sequenced
  normalized events, `next_sequence`, and `terminal`.
* `POST /v1/turns/{id}/cancel` and `/finish`: no body; acknowledgement `{}`. Finish returns
  `cleanup_pending` until provider cancellation/cleanup completes; retry is safe.

Message roles and contents use normalized `ModelMessage`: text, assistant tool calls and matched
text tool results. Tool execution stays with the calling agent, not the broker. Definitions and
schemas are bounded, external schema references rejected, and tool selection/schema capabilities
must be guaranteed by both fixed model and provider. Images are unsupported. Unknown/private
message fields, model/profile/auth/environment/provider settings cannot cross the HTTP contract.
Output includes normalized tool-call events, text, usage and terminal events. Errors retain only
normalized category/retryability with a generic safe message; private metadata, reasoning,
continuation/context records and upstream diagnostics are excluded.

Start reserves the request identity before dispatch. Identical requests under the same lease
return the same turn ID; changed payloads conflict. Start acknowledgement means **reservation**,
not provider success. Poll reports normalized failure without upstream payloads. Ambiguous starts
are never repeated automatically; their reservation remains capacity-accounted until restart.

Cursors replay retained events; an evicted/future cursor returns `cursor_gap` (410), not an
incomplete successful stream. Retention is process-local: neither leases nor turns survive restart.
Expired scopes cannot read or mutate turns. Provider cancellation and finish run independently of
HTTP connectivity. Cleanup failures remain accounted rather than silently evicted. Target discovery,
start and lifecycle calls have host deadlines (30/60/30 seconds). A timed-out start remains ambiguous
and is never reissued. Configure bounded provider-native network timeouts too; dropping a host await
cannot prove remote work stopped. Shutdown waits for admitted lifecycle workers.

## Validation

Behavioral tests cover strict wire decoding, bearer ambiguity, exact workflow/repository claims,
algorithm rejection before key fetch, lease capacity/expiry/JWT shortening, host ownership/idempotency,
cursor gaps and terminal page draining. Direct host integration tests load the real bundled fake
provider and exercise normalized tool continuation, cancel/finish acknowledgement, invocation loss,
expiry, shutdown and failed-start reservation retention. These offline tests do not establish real
provider or GitHub deployment acceptance. Coordinator owns formatting/build/test runs and integrated
provider/HTTP verification.
