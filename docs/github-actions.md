# GitHub Actions and headless automation

Bcode accepts arbitrary prompts through the Rust `run` command. The setup and direct run adapters are thin Bash wrappers; the optional broker OIDC
exchange additionally uses Python 3's standard library (no third-party packages).
This initial profile is **trusted code only**, not an OS sandbox.
Do not run fork-controlled code, workflows, configuration, or prompts with credentials.

## First run

Build/publish a distribution containing the new `run` command and matching bundled
plugins through the existing release tooling. An older released binary does not
acquire this command merely by using the new Action.

Configure these repository variables:

* `BCODE_ACTION_RELEASE_URL`: HTTPS URL for the reviewed Linux tar.gz distribution.
* `BCODE_ACTION_RELEASE_SHA256`: independently reviewed SHA-256 of that distribution.
* `BCODE_ACTION_MODEL`: explicit OpenAI API model ID available to your account.

Set repository/environment secret `BCODE_ACTION_API_KEY` to an OpenAI API key.
This explicitly uses Platform API billing, not ChatGPT subscription billing.
Run **Bcode prompt** (`bcode-prompt.yml`) with your task and execution budget.
The workflow must be on the default branch for GitHub's normal dispatch interface.
No publication credentials, auth JSON synchronization, or secrets-writing PAT is needed.

The workflow generates a non-secret configuration in private runner storage,
passes prompt text as data, installs a verified distribution and invokes the reusable
run Action. Only an observed completed turn returns success. Unknown outcome is
failure, not permission to rerun effects. Permissions/interaction waits fail rather
than opening a terminal prompt; configure permitted tools in reviewed configuration.

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
Do not grant `id-token: write` or publication credentials to this workflow. Checkout
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
no redirects. Only the opaque lease reaches the provider environment. JWTs,
lease responses, diagnostics and generated model text are never printed to CI logs.
Private token files are removed on exit; broker expiry reaps remote turns after
runner loss. Execution budget must fit inside the returned lease minus ten seconds;
the broker currently caps leases at 300 seconds. A 600-second default must therefore
be overridden. Use ephemeral trusted runners; inherited secrets are not sandboxed.

The client now forwards bounded normalized tool definitions, selection/schema policy,
assistant tool calls and text-only tool results. The broker negotiates them against its
fixed admitted model; unsupported tools fail closed. Execution occurs on the local runner
under ordinary Bcode permission policy, not on the broker. Configure permissions deliberately:
OIDC does not grant tool authorization. Images, provider extensions and reasoning remain
unsupported, and no live general-agent/deployment acceptance is claimed.
Direct mode remains unchanged
and retains explicit API-key billing. Neither mode silently falls back to the other.
Python 3 is required only for broker mode. Offline broker client/exchange tests are
not evidence of real GitHub OIDC deployment or provider permission/support.

## Verification status

Adapter tests cover pinned installation/rejection, prompt-as-data and exit propagation.
Rust tests cover bounded input, exact-turn settlement, cancellation and selection.
These are not evidence of a live GitHub provider run. Before treating the integration
as accepted, install a current distribution, configure authorized credentials, dispatch
a useful prompt and inspect its real terminal result; also check timeout/failure.
