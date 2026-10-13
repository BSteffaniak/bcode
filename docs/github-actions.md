# GitHub Actions and headless automation

Bcode accepts arbitrary prompts through the Rust `run` command. The setup and run adapters are thin Bash wrappers; the optional broker OIDC exchange and private configuration preparation are owned by the Rust broker-provider plugin.
This initial profile is **trusted code only**, not an OS sandbox.
Do not run fork-controlled code, workflows, configuration, or prompts with credentials.

## First run

The dispatch workflow builds Bcode directly with Cargo and reuses the binary through
pinned `cache-artifact`; no published Linux distribution is required. The cache identity
includes source content, the locked dependencies, runner platform, feature-set version,
and Rust toolchain identity. The trusted binary and agent configuration are copied outside
the checkout before execution. Only trusted maintainer dispatches may use this cache.

Choose `broker-oidc` (default) or `direct` explicitly:

* Broker: configure `BCODE_ACTION_BROKER_URL` and `BCODE_ACTION_BROKER_AUDIENCE`.
  Deploy and enroll the single-owner broker as described in [Fly.io setup](github-actions-fly.md).
  The broker allowlist must match this repository's immutable ID, workflow path, ref and subject.
* Direct: configure `BCODE_ACTION_MODEL` and secret `BCODE_ACTION_API_KEY`.
  This explicitly uses Platform API billing, not ChatGPT subscription billing.

Run **Bcode prompt** (`bcode-prompt.yml`) with your task and execution budget.
The workflow must be on the default branch for GitHub's normal dispatch interface.
Broker mode requests a GitHub OIDC token; it does not receive the API key or subscription
refresh credentials. Neither mode silently falls back to the other. The short default
budget is 120 seconds; broker preparation rejects budgets exceeding the actual lease.
No publication credentials, auth JSON synchronization, or secrets-writing PAT is needed.

The workflow passes prompt text as data and invokes the reusable run Action.
Only an observed completed turn returns success. Unknown outcome is failure, not permission
to rerun effects. Permissions/interaction waits fail rather than opening a terminal prompt.

## Unattended agent policy

The reviewed `.github/bcode/ci-agent.toml` uses the ordinary `[agent.build]` policy
schema, like local build/plan configuration. It allows checkout filesystem edits and a
small set of validation/read commands, denies external-directory access, and disables
interactive questions and workflow delegation. It is not a permission bypass or sandbox:
Cargo executes trusted project build scripts/tests, and shell commands can run project code.
Review this policy for the actual task; denied or approval-dependent work fails closed.

The run Action accepts `agent` and an absolute `agent-config` outside the checkout.
It combines that trusted policy with either auth configuration. `run --agent` selects the
registered profile through the session application API before submitting the prompt.
Use `plan` plus an appropriate reviewed policy for read-only tasks; do not copy private
user config or credentials into CI. Filesystem layout and provider OIDC remain separate
from agent authorization.

## Reuse outside this repository

Pin both Actions to a reviewed full commit SHA. First use `setup-bcode` with a
trusted archive and independent digest. Then use `run-bcode` with its binary output,
an absolute trusted configuration outside the checkout, explicit profile, prompt,
and timeout. Credentials are supplied via the environment names declared by that
configuration. The Action does not select an account or switch billing modes.

For other CI systems, the equivalent portable interface is:

```sh
/path/to/verified/bcode --config-only /trusted/bcode.toml --profile ci \
  --state-root /private/unused-state --no-credential-discovery --json-errors \
  run --cwd /trusted/workspace --file /private/prompt.txt --timeout-secs 600
```

The state path must not already exist; Bcode creates it privately before startup.
`--stdin` replaces `--file`. Results use the exact-turn versioned JSON envelope.
See [scripted observation](scripted-turn-observation.md) for terminal outcomes and
unknown-admission semantics. The execution budget excludes initial config/bootstrap;
cancellation has a separate bounded grace. Hard kill/SIGTERM may prevent result output.

## Trust, output and cleanup

The API key is available to the trusted Bcode process and may be inherited by tools.
Environment filtering is not credential isolation: arbitrary same-user code may
inspect processes, files or the runner. Use ephemeral runners and trusted maintainers.
Broker mode requires `id-token: write`; restrict it to trusted maintainer workflows
and enforce the exact broker allowlist. The cache step needs `actions: write` for artifact
publication; the run step does not inherit its token. Do not add repository publication
credentials to the agent environment. Checkout
uses `persist-credentials: false`. No PR/fork triggers are supplied.

The run Action retains private result and diagnostic files for the current job,
returns the result path and exit code, and does not print raw model text or diagnostics
into workflow logs. These files can contain sensitive source/model content: no automatic
artifact uploads are provided. Successful runs attempt idle-daemon shutdown. Unknown
active work is not force-killed; runner disposal is final cleanup. The Action is not
supported for unattended persistent shared runners with its disposable-state profile.

## Subscription credentials

Existing Bcode provider integrations can refresh an enrolled owned subscription
profile and persist rotated credentials in custody. A persistent trusted single-user
installation, with unattended access to custody/unlocking dependencies, is the smallest
candidate for avoiding job-by-job credential copying. Do not put refresh bundles in
GitHub secrets/artifacts or try to persist rotation by updating secrets.

This is **not currently a verified supported OpenAI subscription CI path**. Provider
support for Bcode automation must be established before enabling it. Revocation can
still require administrator reconnection; no system can promise perpetual auth.
See [provider assessment](github-actions-auth-support.md). A shared gateway, hosted
service, multi-tenant broker and GitHub App are not prerequisites for this initial
trusted direct-API integration. The optional single-owner broker client is described
below; no hosted service, external TLS deployment or live acceptance is claimed.

## Operator custody setup status

The explicit local command is `bcode auth broker-setup <initialize|reconnect|readiness>`
with `--provider`, `--profile`, `--directory`, and `--identity-file`; initialization
also requires `--recipient`. The directory and identity file must be absolute,
operator-controlled paths. The identity file contains one owner-only age identity;
never pass its value on the command line. The profile must pin the enabled provider
plugin, subscription method, sshenv backend, and explicit `device_seal = "off"`.
Initialization requires Linux and interactive `CONNECT` confirmation; reconnect
requires stopping the broker, settling prior work, and `RECONNECT` confirmation.
Readiness is local and non-mutating, not proof of remote provider support.

The pinned sshenv vault supports native age recipients without a hardware/plugin feature.
Initialization creates encrypted custody through its typed APIs; it does not copy legacy
vault ciphertext or substitute another custody policy. Local tests cover fresh initialization,
fenced enrollment publication and readiness after reopen. These are not proof of live Linux
operator enrollment or subscription provider permission/support. Direct API mode does not
close the separate subscription acceptance gate.

## Optional scoped broker OIDC mode

Use a distribution bundling `bcode.auth-broker-provider` and a trusted deployed HTTPS
broker origin. Broker server admission must already bind the upstream model/profile,
account-use authorization, custody and exact GitHub repository/workflow/ref/subject policy.
The Action never enrolls or receives subscription credentials.

```yaml
permissions:
  contents: read
  id-token: write
steps:
  # Install reviewed matching binary first (outside checkout), then:
  - uses: ./.github/actions/run-bcode
    with:
      binary: ${{ steps.setup.outputs.binary }}
      auth-mode: broker-oidc
      broker-url: https://broker.example
      broker-audience: https://broker.example/actions
      prompt: Review the checkout using explicitly permitted tools.
      timeout-secs: '120'
```

Omit `config` and `profile` in this mode: the Action writes an exclusive non-secret
`github-broker` profile selecting `bcode.auth-broker-provider` / `broker-scoped`.
It obtains the GitHub issuer token from runner OIDC request environment as data,
adds the audience via URL encoding, and exchanges it at `POST /v1/leases` with
no redirects. Only the opaque lease is stored in owner-only private files read by the provider;
tool subprocesses do not inherit a lease environment variable. JWTs,
lease responses, diagnostics and generated model text are never printed to CI logs.
Private token files are removed on exit; broker expiry reaps remote turns after
runner loss. Execution budget must fit inside the returned lease minus ten seconds;
the broker defaults to 900 seconds and rejects configured leases above 900 seconds.
Verified GitHub JWT expiry can shorten the actual lease. A 600-second execution budget
requires at least 610 seconds remaining; otherwise preparation fails before model start.
Broker mode rejects longer budgets instead of renewing or replaying model/tool effects.
Direct mode retains the separate bounded execution budget. Use ephemeral trusted runners;
inherited secrets are not sandboxed.

The client now forwards bounded normalized tool definitions, selection/schema policy,
assistant tool calls and text-only tool results. The broker negotiates them against its
fixed admitted model; unsupported tools fail closed. Execution occurs on the local runner
under ordinary Bcode permission policy, not on the broker. Configure permissions deliberately:
OIDC does not grant tool authorization. Images, provider extensions and reasoning remain
unsupported, and no live general-agent/deployment acceptance is claimed.
Direct mode remains unchanged
and retains explicit API-key billing. Neither mode silently falls back to the other.
Broker mode requires a distribution bundling the broker-provider plugin's
`auth-broker prepare` Rust command; Python is not required. Offline Rust exchange and
shell adapter tests are not evidence of real GitHub OIDC deployment or provider permission/support.

## Verification status

Adapter tests cover pinned installation/rejection, prompt-as-data and exit propagation.
Rust tests cover bounded input, exact-turn settlement, cancellation and selection.
These are not evidence of a live GitHub provider run. Before treating the integration
as accepted, install a current distribution, configure authorized credentials, dispatch
a useful prompt and inspect its real terminal result; also check timeout/failure.
