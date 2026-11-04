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
acknowledgments cannot supply that mapping. Workflow settlement requires both this
canonical, boundedly indexed correlation and authoritative native lifecycle evidence.
The current Claude integration cannot prove this mapping and therefore cannot complete
workflow assignments; it does not manufacture acceptance from result or assistant IDs.
Codex emits the mapping only from verified native turn-start/steering acknowledgments.

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

## Compatibility and verification

The independently evolving service interface is `bcode.harness/v2`; v1 plugins
are not admitted through the v2 registry. The envelope shape remains schema one.
V2 distinguishes native-turn acceptance, consumed-prompt attribution, result
attribution, terminal disposition and background-work settlement. None substitutes
for another. Operation
requests and responses validate correlation, operation family, instance, generation
and bounded payloads. Durable session-event changes follow the existing migration
and writer-epoch mechanisms rather than a parallel event database. Event schema
54 / writer epoch 19 fences the new closed observation variants and indexes;
existing epoch-18 data requires the explicit registered migration.

Deterministic protocol fixtures and isolated application tests supplement, but do
not replace, exact-version native acceptance. Native login behavior, background
continuation, interruption and process-loss reconciliation require explicit release
evidence. Unavailable executables, missing accounts and unapproved spending remain
reported acceptance blockers; fixture success does not establish live compatibility.
