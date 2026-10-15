# Single-owner auth broker on Fly.io

This is a **self-hosted Linux source-build deployment**, not a hosted service or a
claim that a provider permits subscription sharing. The operator must establish
account/provider permission before enrollment or model work. Nothing in this guide
is authorization to deploy, spend money, install secrets, or enroll an account.

## Files and source prerequisites

* `deploy/auth-broker/Dockerfile`: builds from the repository with
  `cargo build --locked --release -p bcode --features app,static-bundled-plugins`.
  No published release is needed. The builder pins `RUSTUP_TOOLCHAIN=1.98.0`
  explicitly so repository `channel="stable"` does not select another toolchain.
  The runtime contains the executable, CA roots,
  Bash, tini, and standard Linux process/file utilities; no Rust toolchain or
  dynamic plugin build/install step. Bundled auth-broker and provider plugins are
  compiled into this executable. `Dockerfile.dockerignore` allowlists build inputs;
  keep private files outside source trees even when ignored. Use the same
  binary/config/state root for daemon,
  client, enrollment, and probes.
* `deploy/auth-broker/supervise.sh`: tini forwards SIGTERM/SIGINT to the Bash
  supervisor. It starts the foreground daemon, probes it, and explicitly runs
  `plugin call bcode.auth-broker/v1 serve --daemon`. It never automatically enrolls,
  reconnects, restarts, or repeats admission. Logs from daemon/client are suppressed.
* `deploy/auth-broker/fly.toml`: maintenance-first template, HTTPS ingress, one
  persistent `/data` volume, immediate (non-overlapping) rollout, restart policy
  `never`, no traffic-triggered starts/stops. It does **not** enforce Machine count.
* `deploy/auth-broker/broker.toml.example`: non-secret config. Replace every `UNSET`
  value and install it at `/data/broker.toml`, root-owned mode `0644`.

Source `plugin call --daemon` and `server startup-probe` now require the existing
daemon, without launching a replacement on connection loss. The supervisor uses
those routes and does not reconnect or replay serve admission. Coordinator still
must validate disconnect, daemon loss during startup/serve, SIGTERM and SIGINT;
source inspection is not runtime or live deployment acceptance.

The supervisor uses the implemented `server stop` command for graceful
shutdown, not OS signals to the foreground daemon (which currently has no signal
hook). It waits for daemon cleanup and the serve client; it does not SIGKILL them.
Fly's 300-second stop timeout is best-effort: interruption/forced infrastructure
termination cannot prove remote work stopped. Treat unfinished exchanges and
cleanup as ambiguous, preserve custody, and review them before an explicit restart.

The ordinary serve IPC timeout is 15 seconds; this launcher explicitly selects a
30-day bounded timeout for the single invocation. Timeout is **not** renewal: the
service stops and requires operator review. Leases remain limited to 900 seconds
and are shortened by GitHub JWT expiration. A cancellation acknowledgement is not
proof of remote termination; only successful cleanup establishes finish.

## Non-secret configuration

Pin the catalog model ID deliberately. The service profile is `broker`, provider
plugin `bcode.openai-compatible`, provider auth ID `openai`, auth profile
`broker-owner`, scheme `chatgpt`, sshenv backend, explicit `device_seal="off"`, and
no auth pool. These service-local names do not select or reuse the operator's
interactive CLI credentials. Do not copy an existing workstation vault or infer
credentials from environment variables.

Set `supported_account_use=true` only after account/provider permission is known.
Pin the exact OIDC audience (for example `https://YOUR_APP.fly.dev/actions`), numeric
GitHub repository ID, `owner/repository`, workflow path **and ref**, git ref, and
subject. For environment-based jobs the subject differs; enter its exact value.
For reusable workflows add the exact `job_workflow_ref` when required. No wildcards,
missing identities, untrusted fork/branch grants, or API-key fallback. Match the
Action's audience exactly; broker URL is the HTTPS origin, not the audience path.
The launcher refuses unresolved `UNSET` values; broker admission performs the
actual typed policy validation.

Bind `0.0.0.0:8080` only behind Fly's TLS ingress with
`trusted_tls_ingress=true`. Do not publish raw port 8080 or permit untrusted private
network callers. The broker validates OIDC itself; a proxy/TCP check is not
permission, custody readiness, or provider readiness.

## Operator bootstrap (explicitly authorized operations only)

1. Choose app/region, substitute the Fly template, and validate it using the
   installed Fly CLI's configuration validation. Build context must be repository
   root. Example build command, **not run by this implementation**:
   `docker build -f deploy/auth-broker/Dockerfile -t bcode-auth-broker .`.
   Verify runtime linked libraries with `ldd /usr/local/bin/bcode`; do not claim
   Linux build acceptance until the coordinator builds/tests this image.
2. Provision exactly one Machine and one `broker_data` volume in the same region.
   Fly Launch/deploy can create redundant Machines by default: disable HA creation
   using the installed CLI's supported `--ha=false` option and verify Machine and
   volume inventories. No autoscaler, clone, standby Machine, shared custody mount,
   blue/green or rolling overlap, release-command enrollment, or parallel deploy.
   Maintenance mode intentionally fails TCP health; bootstrap it without requiring
   a successful serving check. Never change checks to conceal serving failures.
3. Generate an age identity outside the repository using your approved secret
   workflow. Retain its **public** recipient separately. Store the single-line
   identity as Fly secret `BCODE_BROKER_AGE_IDENTITY` via stdin, not a command-line
   value. For a private identity file, a Bash example is:

   ```bash
   set +x
   { printf 'BCODE_BROKER_AGE_IDENTITY='; tr -d '\r\n' < /PRIVATE/identity; printf '\n'; } |
     fly secrets import --stage --app YOUR_APP
   ```

   This is a secret installation operation requiring operator authorization.
   `--stage` defers application until the next authorized deployment. Never use
   `fly secrets set NAME=$(cat ...)`, shell tracing, build args, Docker COPY,
   checked-in files, GitHub outputs/artifacts, or logs for private identities.
   Confirm installed CLI stdin/staging support first. The supervisor unexports the
   secret before external commands, writes it using a shell builtin to ephemeral
   `/run/bcode`, and gives the regular identity file UID 10001/mode 0600. Existing
   staged identity must match; silent rotation is rejected. `/data` directories are
   owner-only. Do not upload custody backups to public build contexts/artifacts.
4. Start the single Machine in `BCODE_BROKER_MODE=maintenance`. Upload only the
   reviewed non-secret config to `/data/broker.toml`; set root ownership and 0644.
   Maintenance starts no daemon/listener/provider exchange and holds no custody
   ownership. Use an interactive SSH console in that same Machine for enrollment.
5. Invoke the actual setup command through the launcher (which drops to UID 10001
   and preserves the same config/state root). The recipient is public:

   ```text
   /bin/bash /opt/broker/supervise.sh initialize --recipient age1YOUR_PUBLIC_RECIPIENT
   /bin/bash /opt/broker/supervise.sh readiness
   ```

   The first maps to:

   ```text
   bcode --config-only /data/broker.toml --profile broker --state-root /data/state auth broker-setup initialize --provider openai --profile broker-owner --directory /data/custody --identity-file /run/bcode/age-identity --recipient age1YOUR_PUBLIC_RECIPIENT
   ```

   Enrollment requires an interactive terminal and literal `CONNECT`; provider-owned
   flow handles credentials. No tokens, passwords, identity values, or flow states
   are arguments. `auth login` is not this retained-custody setup. The pinned sshenv
   age-only wrapping fix must be integrated and Linux-tested; initialization failure
   is maintenance-required, **not** permission to construct ciphertext, delete the
   directory, weaken device policy, or retry an ambiguous exchange.
6. Readiness is local only. `auth broker-setup readiness` exits zero only for Ready;
   Empty and ReconnectRequired exit nonzero. The launcher uses that status and
   suppresses stdout/stderr; it does not parse presentation text or attempt repair.
   Readiness takes the custody owner lock, so run it only while serving is stopped.
7. Once source/image validation and readiness succeed, deliberately switch the
   reviewed Fly environment to serving.
   Perform one immediate deployment without HA, verify exactly one Machine, and
   require its TCP check to pass. Do not automate this promotion or enrollment.

For reconnect, stop serving, settle/cancel prior provider work, enter maintenance,
then run `/bin/bash /opt/broker/supervise.sh reconnect` interactively and confirm
`RECONNECT`. The launcher serializes operator operations against serving using
`/data/operator.lock`; the provider custody owner is independently exclusive.
Never run reconnect against a second Machine while the old owner may still exist.
Do not restore an old volume snapshot and assume previously consumed tokens are
valid. Preserve damaged/ambiguous state for explicit maintenance.

## GitHub Action handoff

Use the repository's source-built action/workflow and select its **broker** mode
explicitly; the coordinator owns those files. The runner requests GitHub OIDC with
`id-token: write`, matching the configured audience, workflow/ref/repository and
subject. Use `contents: read` unless the task explicitly needs stronger permissions.
The agent profile is `build`; tool side effects still require policy authorization.
There is no fallback to direct auth or replay after an ambiguous broker start.
Direct mode is a separate explicit choice, not broker recovery. Do not place the
owner's subscription credentials or age identity in GitHub Actions secrets.

## Acceptance checklist (coordinator/operator, not performed here)

* Build locked source image and verify static bundled broker/provider registration,
  Linux library availability, exact artifact identity, and client/daemon compatibility.
* Verify sshenv age-only initialization, explicit setup confirmation, failed/ambiguous
  enrollment preservation, readiness Empty/Ready/ReconnectRequired, and ownership.
* Assert unset config, missing/unsafe identity, unsupported profile/policy, duplicate
  operator launch, and daemon loss fail closed without replacement/admission retry.
* Offline host tests: invocation disconnect, expiry, cancel/finish, cleanup failure,
  shutdown drainage, and stable terminal outcomes. No stale events reopen completion.
* Supervised start: only one foreground daemon and one serve invocation; TCP port
  becomes reachable. There is **no implemented `/health` or `/health/ready` route**;
  the Fly check is TCP only and does not verify upstream access.
* Send SIGTERM and SIGINT to tini; observe graceful stop IPC, drain, both children
  exit, listener closure, and no restart. Test early startup signal/daemon failure.
  Test a too-short forced stop separately and retain ambiguity, not false success.
* With separate approval for real provider work: exact GitHub OIDC allow/deny cases,
  model/tools continuation, cancellation, lease expiry and secret-safe diagnostics.
  Offline tests and these files do not establish live provider permission/support.
* Before every rollout/restart: confirm one Machine, exclusive volume/custody owner,
  no unresolved work, no deployment concurrency, explicit operator approval. Secrets,
  enrollment responses, bearer tokens and custody are absent from logs/artifacts.

Fly settings follow its [configuration reference](https://fly.io/docs/reference/configuration/).
Verify the installed Fly CLI/version before an authorized operation; this guide is
not a report of an executed deployment.
