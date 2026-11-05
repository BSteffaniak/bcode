# Managed external session architecture

## Boundary and ownership

An external harness is an execution backend for an ordinary Bcode session, not a
model provider. Session parentage, workspace selection and canonical history retain
their existing ownership. Unrelated native conversations remain import-plugin scope.

The application boundary admits interactive, tool, command and workflow operations.
It resolves the configured harness instance and verifies execution ownership before
calling the native plugin. Selecting a session or naming a parent is not authority.
Plugins interpret native protocols and capabilities; they do not schedule Bcode tools
from observed native tool calls. Native subagents are observations unless a separate
application operation explicitly creates an independently controlled Bcode child.

Portable contracts live in `bcode_harness_models`, plugin registration/routing in the
plugin domains, and canonical external evidence in session models and persistence.
`bcode_claude_code` is a protocol/process library independent of Bcode's application,
persistence and workflow implementations. Claude Code and Codex integrations are
separately disableable plugins, as is the generic managed-session tool plugin.

## Identity and evidence

Do not collapse these identities:

* Bcode session and immutable parent association;
* configured harness instance and credential/configuration namespace;
* native conversation;
* submission, attempt, native turn/run and mutation;
* execution ownership generation and connection generation;
* outstanding interaction and its response.

Transport preparation and native identity confirmation are distinct. A successful
control initialization may retain a requested native UUID without proving a native
conversation exists. The first admitted submission can use that prepared live
transport; only matching native identity evidence promotes it to a confirmed
binding. Preparation never authorizes resume, replay after process loss, or an
artificial prompt to force identity discovery.

Execution, connection health, attention, continuation and observation completeness
are separate facts. An empty observation page is not settlement. Neither native
result delivery, interrupt acknowledgment, quiet time nor process exit independently
proves that background work has settled. Unknown native support remains unknown or
constrained; capability declarations must not infer support from a vendor name.

`SubmissionAccepted` records native proof linking an exact submission and attempt
to a native turn, under the enclosing observation's session and generation. Request
acknowledgments cannot supply that mapping. Codex supplies genuine native turn
identities; Claude's prompt/result UUIDs identify native frames, not native turns.
The legacy strict settlement path retains its stronger correlation requirements.

### Separated lifecycle contract

The `bcode.harness/v3` interface adds `Execution` observations without changing the
schema-one request envelope. Schema 55 / writer epoch 20 fences their durable
representation; earlier history is converted by session-migration and does not
acquire invented lifecycle evidence.

* `SubmissionTerminal` retains the exact Bcode submission and attempt, native
  terminal evidence, disposition, and complete output when available. A genuine
  result frame suffices structurally without fabricating a native turn identity.
* `BackgroundWork` retains adapter-owned work identity, optional genuine native
  task identity and exact origin, state, parent relationship, and notification
  status. Completed work can still owe a completion notification.
* `Continuation` identifies a distinct automatic native follow-up and its causal
  origin. It cannot reopen the terminal foreground attempt or authorize Bcode
  worker dispatch. Native turn identity remains optional, never synthesized.
* `Coverage` describes supported tracking through the current observation. It is
  not a no-future-work certificate, does not erase retained work or notifications,
  and cannot make an unobserved native activity class authoritative.

Callers choose a completion boundary: foreground, associated work (the default),
or workspace release. Foreground requires authoritative exact-attempt outcome and
complete attributed output, not universal quiescence. Associated work additionally
requires complete relevant tracking, resolved background work, notifications and
required continuations. Uncorrelated outstanding work fences session-wide safety;
unknown tracking is not an empty authoritative roster. Workspace release additionally
requires host-owned release authority; coverage alone cannot establish it.
Background failure can fail an enclosing assignment while its foreground success
remains unchanged. Native continuation execution and explicit asynchronous handoff
must retain responsibility separately; neither may be inferred from idle or a
successful response.

These are compatibility and implementation contracts, not a claim of completed
native, workflow, or frontend acceptance. Each adapter's supported tracking and the
production persistence/policy journey require separate verification.

## Canonical delivery and recovery

The session writer persists ownership-fenced mutation intent before native delivery.
An identical retry returns recorded evidence when available. Conflicting reuse of an
identity fails. If delivery or acknowledgment is ambiguous, retain uncertainty and
reconcile; never repeat the native mutation merely because a client disconnected,
a timeout elapsed or a daemon restarted.

Normalized observations are appended/deduplicated through the canonical session
writer before publication. Projections and cursors are disposable derived data, not
a plugin-owned recovery journal. Routine history and current-state access are bounded
and indexed. Missing or incompatible projections report degraded/maintenance state;
normal attach and catalog paths do not silently replay or repair history.

Native plugin availability is independent of stored-history availability. Disabling
a plugin or losing a native executable must not erase canonical evidence or prevent
unrelated sessions from opening. Resuming native work requires current ownership,
compatible exact native identity and supported reconciliation semantics.

## Interactions and cancellation

Questions, approvals and requests for assistance have different authority semantics.
Responses bind the exact pending interaction, admitted caller, native binding and
ownership generation. Stale, mismatched or already-consumed responses fail closed.
A native request is not itself permission to execute a Bcode operation. Parent
assistance requires separately admitted authority rather than inherited parentage.

Cancellation retains its canonical intent and dispatch receipt. Native interruption
is an attempt whose acknowledgment is not authoritative settlement. Unconfirmed
effects remain visible as uncertain; workflow completion requires authoritative
execution evidence, not merely a session identifier or a native result message.

## Authentication and billing

Provider-auth owns custody and namespace-level coordination. Supported custody modes
are native-managed, vault-managed and explicitly selected external custody. A native
account must not be represented by an empty vault profile. Secret access remains
plugin-scoped; account selection must not silently fall back to a different billing
route.

Daemon-owned sensitive flows are ephemeral and bounded, shared only by the actual
credential namespace with independently cancellable waiters. Reconnect may recover
a still-live flow; restart requires fresh readiness inspection, not replay of a
callback or login mutation. Sensitive URLs, codes and input never enter ordinary
session history, model context, tool traces or workflow payloads. Readiness status,
refresh and remote verification are distinct operations.

Plugins may explicitly declare the query service `bcode.harness.terminal-auth/v1`
(`TERMINAL_AUTH_INTERFACE_ID`). Its `prepare_login` operation accepts the existing
schema-one `{schema_version, request_id, instance}` request and returns
`{schema_version, request_id, handoff}`, using the existing verified native login plan.
The owning plugin interprets configuration; the host validates correlation, namespace,
executable, custody and current admission before issuing the plan.

`ManagedSessionCommandHandle::terminal_auth_interface()` returns `Option<String>`
from the enabled service metadata for the canonically bound plugin only. Exactly one
matching declaration is required; duplicates fail closed and absent declarations return
`None`. Discovery grants neither readiness nor permission and does not launch native
programs. Callers must explicitly yield terminal ownership and obtain consent before
`ManagedSessionAuthFlow::terminal_handoff(interface_id)`. Reaping the exact child and
host reinspection remain mandatory; successful exit is not authentication readiness.
Claude additionally retains `bcode.claude-code.handoff/v1` for existing explicit clients;
generic discovery never derives or falls back to a vendor-specific interface.

Claude configuration selection consistently scopes status, login, execution and
resume through the same native configuration directory. Native login handoff belongs
on an explicitly connected client terminal, never daemon-inherited standard input.
Codex uses app-server and expected-turn steering. API-key delivery requires verified
ephemeral native credential storage; persistent fallback is not equivalent.

## Presentation

SessionView owns renderer-neutral external-session semantics. Clients render
canonical observations even when the execution plugin is unavailable. Native text
is untrusted display content, not browser markup, terminal authority or tool calls.
Frontend response controls require current capabilities and exact interaction
correlation. Sensitive auth surfaces remain separate from the ordinary chat composer.
BMUX remains the terminal layout authority.

### Explicit Claude API-key custody

Claude native-account login remains the default (`billing.route = "native"`). API billing is a
separate, explicit route; it never imports native subscription credentials or falls back to them.
The API route requires a `VaultManaged` instance and an exact plugin-owned `sshenv` auth profile.
No key belongs in plugin configuration, workflow JSON, instance catalogs, or command arguments.

Example non-secret configuration (replace paths and verify the installed native version):

```toml
[auth.profiles.claude-api]
backend = "sshenv"
provider_id = "claude-code"
owner_plugin_id = "bcode.claude-code"
scheme = "api_key"

[auth.profiles.claude-api.map.api_key]
key = "ANTHROPIC_API_KEY"

[auth.profiles.claude-api.settings]
vault = "/absolute/path/to/auth-vault"
profile = "stored-api-account"

[plugins.config."bcode.claude-code".instances.api]
executable = "/absolute/path/to/claude"
config_directory = "/absolute/path/to/isolated-claude-config"
exact_version = "<exact installed --version output>"
setting_sources = []

[plugins.config."bcode.claude-code".instances.api.billing]
route = "api_key"
provider_id = "claude-code"
profile = "claude-api"
credential_namespace = '["/absolute/path/to/auth-vault","stored-api-account"]'
```

Enroll the API key through the existing provider-auth secret-field/vault enrollment surface owned
by `bcode.claude-code`; this example contains no secret and does not create a profile or grant.
The generic configured harness instance must use the same instance ID, executable and config
namespace, and this auth binding:

```json
{"provider_id":"claude-code","profile":"claude-api","credential_namespace":"[\"/absolute/path/to/auth-vault\",\"stored-api-account\"]","custody":"vault_managed"}
```

The credential namespace is the compact JSON array of the absolute vault path and actual vault
storage-profile name, not a display label. The profile's declared credential mapping must match
the owning plugin's registered API-key field. Current caller/workflow authorization and canonical
managed-session ownership are rechecked for every credential lookup. Matching configuration alone
never grants access. Missing, changed or revoked ownership, wrong custody/provider/profile/namespace,
unavailable material and cancellation fail closed. Ambient API keys and alternate native billing
routes remain rejected rather than silently selecting another account.

Local API readiness means that exact credential material is available for the selected launch;
it is not remote verification. API launch does not execute native-account `auth status` or login.
The key is injected only into the explicitly selected API process's cleared environment. Native
account status/login continues to use the native configuration namespace unchanged.

The plugin declares `bcode.plugin.sensitive-credentials/v1`. Its invocation-only callback carries
bounded non-secret selection metadata and returns raw secret bytes outside ordinary config,
bridge-response serialization, events and diagnostics. Unknown/older loaders reject the callback;
API execution is unavailable on those hosts, never downgraded to native-account billing. This
capability does not change the native callback ABI or the managed-session IPC schema.

## Compatibility and verification

The independently evolving service interface is `bcode.harness/v3`; older plugins
are not admitted through the v3 registry. The envelope shape remains schema one.
V2 introduced native-turn acceptance, consumed-prompt attribution, result
attribution, terminal disposition and background-work settlement. V3 additionally
separates foreground outcomes, retained work, notifications, continuations and
tracking coverage; none substitutes for another. Operation
requests and responses validate correlation, operation family, instance, generation
and bounded payloads. Durable session-event changes follow the existing migration
and writer-epoch mechanisms rather than a parallel event database. Event schema
55 / writer epoch 20 fences the new closed observation variants and indexes;
existing epoch-19 data requires the explicit registered migration, preserving the
previous epoch-18 migration chain.

Deterministic protocol fixtures and isolated application tests supplement, but do
not replace, exact-version native acceptance. Native login behavior, background
continuation, interruption and process-loss reconciliation require explicit release
evidence. Unavailable executables, missing accounts and unapproved spending remain
reported acceptance blockers; fixture success does not establish live compatibility.

### Opt-in native CLI acceptance

With explicit authorization to use the existing native Claude subscription, run:

```sh
cargo test --locked -p bcode --features app,static-bundled-claude-code-plugin \
  --test harness_native_acceptance -- --ignored --nocapture
```

The test verifies the pinned Claude executable and wrapped executable hashes, uses
native credential custody, and starts a matching client/daemon in private Bcode
state without any model-provider credentials. It checks two sequential submissions,
exact result/attempt correlation, foreground completion and settled activity, then
compares canonical history after daemon restart with the adapter disabled. It does
not establish native login handoff, safe recovery, managed-tool execution, or
background-continuation settlement. `BCODE_NATIVE_CLAUDE_CONFIG_DIR` may select an
existing native credential namespace; credentials must not be copied into test state.

Observation clients must use the exact returned `next_after`, including `None` on
an empty page, and adopt confirmed session bindings. Canonical history has a
separate cursor: an empty page with an unchanged cursor is its current tail, not
proof that future events cannot arrive. Diagnostics emitted on recurring bridge
operations have distinct observation identities; bridge identity is not an event ID.
