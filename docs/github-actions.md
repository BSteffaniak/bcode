# GitHub Actions and headless automation

Bcode accepts arbitrary prompts through the Rust `run` command. The setup and run
Actions are thin Bash adapters; no Python runtime or task-specific prompt framework
is required. This initial profile is **trusted code only**, not an OS sandbox.
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
trusted direct-API integration and are not claimed implemented.

## Verification status

Adapter tests cover pinned installation/rejection, prompt-as-data and exit propagation.
Rust tests cover bounded input, exact-turn settlement, cancellation and selection.
These are not evidence of a live GitHub provider run. Before treating the integration
as accepted, install a current distribution, configure authorized credentials, dispatch
a useful prompt and inspect its real terminal result; also check timeout/failure.
