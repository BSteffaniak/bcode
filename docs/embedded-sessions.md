# Embedded sessions

Applications that embed Bcode, such as bdata's SQL assistant, the standalone review app
(`bcode-review`), or a future desktop or web host, should be able to create real Bcode
sessions. Those sessions should be listable, resumable and observable from `bcode tui` and from
any other Bcode consumer, whether that consumer embeds Bcode or not. This document specifies how
that works. It is a design that needs sign-off before implementation. **Status:** specified, not
built. The decisions that need sign-off are listed at the end.

Related contracts: [session persistence](session-persistence-architecture.md),
[state locations](state-locations.md), [daemon lifecycle](daemon-lifecycle-architecture.md),
[application operation boundary](application-operation-boundary.md), [Rust SDK](sdk.md),
[SDK session persistence](sdk-session-persistence.md), [permissions](permissions.md),
[session import plugins](session-import-plugins.md).

## Goals

* **One session model.** A conversation an app creates is a canonical Bcode session, stored in
  the user's selected state location under the normal persistence rules. It is not a private
  copy.
* **Visible everywhere.** `bcode tui`, `bcode session list`, the review app, bdata and any other
  consumer can list it, read its history, observe it live and continue it, with the usual
  permission and ownership rules.
* **Know where a session came from.** Sessions record the app that created them and a small amount
  of app context. Pickers use it for labels and filters.
* **App-provided tools.** An app supplies tools that run in its own process and see its live state,
  such as bdata's database tools, which need the open connection and the screen. They are
  governed by the same Bcode permission system as plugin tools.
* **Shared permissions.** Allow, ask and deny rules and remembered decisions come from the user's
  Bcode configuration and state. Apps do not keep a separate permission engine or write Bcode's
  state files directly.
* **App and Bcode upgrade independently.** An app built against one Bcode revision keeps working
  against an installed Bcode from a later revision within a declared compatibility window.
* **Existing app data carries over.** Conversations that apps already keep in their own stores,
  such as bdata's JSON files, can be imported once, explicitly and without loss.

## Non-goals

* No second canonical store, and no synchronization between an app store and the Bcode store.
* Embedded hosts do not open `session.db` directly, and nothing bypasses session leases.
* Cross-artifact live observation is out of scope. Two daemons of different Bcode builds do not
  stream each other's sessions.
* The in-process SDK (`AgentSession` with `SessionPersistenceAdapter`) is not removed. It stays
  for stateless generation and app-owned persistence. Its sessions are not Bcode sessions.

## Current state

* **SDK mode.** `Bcode::configured_embedded_with_options` builds an in-process runtime from the
  user's Bcode configuration and static bundled plugins. `Agent::session_with_store` /
  `session_with_persistence` keep a `PersistedSession` (schema 1: id, visible `ModelMessage`
  transcript, persisted memories) in an app-chosen `SessionPersistenceAdapter`. The bundled
  `LocalSessionStore` is one JSON file per conversation, written by temp-and-rename. It is
  single-process, has no catalog, no event history, no leases and no observation, and nothing
  else can see it.
* **bdata today** (`packages/assistant`, `packages/app/src/assistant`) uses SDK mode:
  * Conversations are `<bdata state>/assistant/conversations/<millis>.json`. Inline edits use
    `assistant/inline/<pid>.json`, and deleted conversations wait in `assistant/deleted/` until
    they are purged.
  * Listing scans the folder and decodes every file.
  * Tools are inline `typed_tool_async` tools. They call back into the UI over a channel
    (`HostQuery`) for workspace rows, the selection and app actions, and ask the user for domain
    approvals such as sharing result rows.
  * Plugin tools run under bdata's own `PermissionPolicy`, which re-implements Bcode's layering.
    Remembered rules are written into Bcode's permission state with
    `bcode_config::upsert_agent_permission_rule`.
* **Daemon clients.** `BcodeClient` implements `SessionApplication`,
  `SessionObservationApplication` and `InteractionApplication`.
  `packages/bcode/examples/application_session.rs` creates a session, admits a turn, observes
  bounded semantic snapshots and answers permissions. `BcodeClient::for_installed_bcode` finds the
  user's executable (`BCODE_BIN` or `PATH`), verifies its daemon for this process's state and
  configuration scope, and starts it if needed.
* **The review app** creates daemon sessions with `create_session_in_working_directory` and records
  which review a session belongs to in its own table (`review_assistant_sessions`). Its
  `code_review.*` tools are daemon plugin tools, which find the calling session through the
  workspace context.
* **Embedded servers.** `bcode_server::run_embedded_with_*` runs a full server in-process on a
  caller-owned endpoint without publishing a daemon record. The HyperChad browser host and tests
  use it.

Gaps this design closes:

1. **Exact build match.** A client verifies the daemon's protocol version, writer epoch and event
   schema against its own compiled constants (`DaemonTarget::verify_status`). An app built against
   another Bcode revision therefore cannot talk to the installed daemon. The protocol version
   is 62 and changes often.
2. **No session origin.** Nothing records which app created a session, apart from per-turn
   `TurnOrigin` and execution provenance.
3. **No app-provided tools.** A daemon session can only call plugin tools. Tools that need the
   app's live state cannot run.
4. **No app-provided instructions.** Agents come from configuration and plugins. An app cannot
   give its sessions their own system prompt.
5. **Two stores.** SDK conversations live outside the canonical store, so Bcode cannot see them.

## Ways to run a session

| Mode | Runtime | Store | Visible in Bcode | Use |
| --- | --- | --- | --- | --- |
| **Attached** (default) | The installed Bcode's daemon | The user's canonical store | Yes: list, resume, observe live | Any app on a machine with Bcode installed |
| **App-hosted** (deferred, decision D2) | The app's linked Bcode, run as a daemon of the app's artifact | The user's canonical store | List and resume when idle; no cross-artifact live observation | Apps that ship without an installed Bcode |
| **SDK-local** (unchanged) | In-process `AgentSession` | App-owned adapter | No | Stateless generation, app-owned persistence, short scratch work such as bdata's inline edits |

Attached mode is the model this document specifies. An app never silently switches between modes:
if the installed Bcode is unavailable, the app reports it and may offer SDK-local mode explicitly.
It never falls back to a different state location (see `A session has one canonical storage path`).

## Session origin

### Record

```text
SessionOrigin {
    version: u32,                      // origin contract version
    app_id: String,                    // stable producer namespace, e.g. "bdata", "bcode.code-review"
    app_display_name: Option<String>,  // label for pickers, e.g. "bdata"
    context: BTreeMap<String, String>, // app-owned keys, e.g. {"connection": "prod-replica"}
    visibility: SessionVisibility,     // Visible (default) or Background
}
```

* **Bounds.** `app_id` is 1–64 bytes of `[a-z0-9._-]`. The display name is at most 64 bytes. The
  context holds at most 16 entries; each key is at most 64 bytes and each value at most 256
  bytes. Requests outside the bounds are rejected, not truncated.
* **Secret-free.** Origin is display and filter data. It must not hold credentials,
  connection strings or file contents. The contract states this, and the app is responsible for
  it. Bcode has no general secret detector to enforce it.
* **Canonical.** A new `SessionOriginRecorded { origin }` event is appended at the end of
  `SessionEventKind`, which bumps the event schema. It is written in the same creation sequence as
  `SessionCreated`, following the precedent of `ExecutionSessionCreated`. Origin is immutable once
  recorded; a second record is rejected.
* **Derived copies.** `SessionSummary` gains `origin: Option<SessionOriginSummary>`, carried by the
  catalog row and `manifest.json` as display caches. A missing or stale cache never hides the
  session. Catalog discovery may recover origin by first-event discovery, because the event sits in
  the bounded creation prefix. Discovery never replays the log and never writes partial indexes.
  Sessions created before this change, or whose origin cannot be read, have no origin and are
  shown as native Bcode sessions. Origin is not backfilled.
* **No authority.** Origin never selects storage, routes execution, grants permissions or
  restricts who may open the session. Any authorized client can read, continue, rename or delete
  an app's session. This follows `Aggregated session discovery does not confer authority` and
  `Presentation does not determine execution semantics`.

### Visibility and workspace scope

* Origin sessions are `Visible` by default and appear in `bcode tui`'s picker with an origin chip
  (the display name). An app may create `Background` sessions for scratch work. These are hidden
  from normal pickers but can be inspected directly, the same way background execution sessions
  are. `SessionSummary::is_picker_visible` considers both.
* The canonical working directory stays the session's workspace scope. Apps choose it
  explicitly: the review app uses the repository; bdata uses the directory it was launched from
  (or a configured project directory). The picker's **This directory** / **All directories**
  scopes therefore work unchanged.
* `SessionCatalogQuery` gains `origin: Option<SessionOriginFilter { app_id, context_key_value }>`.
  It filters bounded catalog pages the same way the text filter does. A filtered page keeps the
  existing degraded-coverage status. Sessions whose origin is unreadable do not match an origin
  filter, and the page reports how many candidates were unclassified, so a degraded catalog is
  not mistaken for "no sessions".
* `bcode session list --origin <app_id>` and the TUI picker filter (`origin:bdata`) use the same
  query.

## One canonical store, one writer per session

* Attached apps create and change sessions only through the daemon's application boundary. The
  daemon holds each loaded session's lease and write lock (`session-persistence-architecture.md`,
  "Lock order and ownership"). Apps never open `session.db`, the catalog or manifests.
* Many clients may attach to one session. Writes are serialized by the session actor, as they are
  today for several TUI windows.
* SDK-local sessions never touch the canonical store. The only way from an SDK store to the
  canonical store is the explicit import below.
* An app-hosted runtime (decision D2) would be a daemon of a different artifact on the same state
  location. It would rely on the existing artifact coexistence rules: leases fence conflicting
  owners, and writer-epoch checks fence incompatible writers. It must also never run automatic
  session migration in the shared location. An app-hosted runtime built from a newer Bcode would
  otherwise migrate sessions and fence the user's installed, older Bcode. A session that needs
  migration is reported as maintenance-required, with a pointer to `bcode`.

## Connecting: the session bridge

The app-to-Bcode boundary has to tolerate version skew. Today's client protocol is an internal,
exact-match contract (gap 1), and it should stay that way, because exact artifact matching is
what makes daemon replacement and recovery safe (`Daemon artifact versions are isolated`).

**Design (decision D1).** The installed Bcode provides a **session bridge**: `bcode session-bridge
--stdio`, a subprocess the app starts from the verified installed executable
(`installed_bcode_executable`, with `BCODE_BIN` honored).

* **Matches its daemon.** The bridge is the installed artifact, so it is an ordinary exact-match
  client of its own daemon. It uses the default auto-start and transparent-reconnect policy.
* **Versioned protocol.** The bridge speaks a separately versioned JSON-lines protocol on stdio
  (`BRIDGE_PROTOCOL_VERSION`, in a domain models crate,
  `packages/session-bridge/models`). `hello` negotiates the highest version both sides support
  within the bridge's declared window, which is the current version and the previous two. Requests
  deny unknown fields. Responses may gain optional fields within a version, and clients ignore
  fields they don't know. Changing the meaning of a field requires a new version. An unsupported
  version fails `hello` with an actionable message ("update bdata" or "update Bcode").
* **Thin client.** The app links `packages/session-bridge` (`bcode_session_bridge`), which holds
  the bridge models, process management, request correlation and typed tool registration. It
  depends on serde and tokio, not on the Bcode runtime, plugins or providers. The models crate is
  a leaf.
* **Lifetime.** The bridge lives as long as its stdin. EOF closes its daemon connections, drops
  the app's tool registrations and detaches its sessions. Turns already admitted keep running,
  because closing a client does not cancel work.
* **Stable view.** Observation through the bridge returns a bridge-owned, versioned projection of
  `SessionViewSnapshot`: transcript items (user, assistant, reasoning, tool call with status and
  bounded arguments and result, permission, interaction), text streams, pending permissions,
  thinking state, usage and cost. A newer Bcode's unknown item kinds map to a generic `other` item
  with display text, so older apps degrade instead of failing.
* **In-tree apps.** Apps built in the Bcode workspace, such as `bcode-review` and `bcode-git`,
  ship in lockstep and may keep using `BcodeClient` directly. Every bridge operation is an adapter
  over a daemon application operation; the bridge has no behavior the daemon lacks.

Bridge operations, all bounded:

* `hello`;
* sessions: `create` (name, working directory, origin, agent, model), `list` (catalog page with
  query and origin filter), `summary`, `history_page`, `observe` (snapshot plus ordered updates;
  resync means a fresh observation), `submit` (text, context blocks, model, effort, agent,
  permission mode), `cancel_turn`, `rename`, `delete`;
* permissions: `pending`, `resolve` (approve or deny, remember);
* tools: `register`, `claim_session`, `release_session`; the bridge also sends `invoke` and
  `cancel` to the app, and the app answers with `result`;
* catalog data: `agents.list`, `models.list` (with pricing and effort values), `usage`;
* `import_sdk_session` (see migration);
* `open_command`: the argv that opens a session in `bcode tui` with the verified executable.

## Discovery, listing and resume

* **Apps** list their own sessions with `list` and `origin = <app_id>`, plus optional context
  filters such as bdata's connection. This replaces folder scans. Resuming is `observe` plus
  `submit` on the chosen session id.
* **`bcode tui`** lists every visible session, including app sessions with their origin chip. The
  transcript renders through the normal session view; app tool calls use the generic tool
  presentation (renderer-neutral fallback). Continuing an app session in `bcode tui` works: while
  the app is not connected its tools are not offered (see below), and old calls stay in history as
  inert records.
* **Other apps** can list, read and continue sessions with any origin. For example, the review app
  could show bdata sessions about the same project. Origin is a label, not an access boundary.
* **Open in Bcode.** Apps show "open in Bcode" (the review dock's `ctrl+o`) with the argv from
  `open_command`, which targets the same state and configuration scope.

## App-provided tools

### Registration

`tools.register { provider_id, tools: [ClientToolDescriptor] }` is sent on the bridge (and by
in-tree apps on a retained daemon connection).

```text
ClientToolDescriptor {
    name: String,                         // "<provider_id>.<tool>", e.g. "bdata.run_query"
    description: String,
    input_schema: JSON Schema,
    policy: ToolPolicyOperation,          // read-only, mutating, external, command, ...
    presentation: Option<ToolPresentation>, // display data only
    max_duration_ms: u32,                 // bounded by the host's configured ceiling
}
```

* **Names** must use the provider's namespace. A collision with a plugin tool, or with another
  live provider's tool, rejects the whole registration. Nothing is shadowed.
* **Descriptor limits:** 128 tools per provider, 64 KiB per schema, 4 KiB per description.
* **Lifetime.** The registry is in daemon memory, keyed by connection. It is not durable and
  disappears with the connection.

### Which sessions see the tools

* **Claims.** A provider's tools are offered only in sessions that a live connection of that
  provider has claimed (`claim_session`). Creating a session with `origin.app_id == provider_id`
  claims it. An app may also claim other sessions explicitly; for example, the review app could
  claim a chat session to offer `code_review.navigate` there. Unclaimed sessions, such as
  ordinary `bcode tui` chats, never see app tools.
* **Several instances.** If several instances of one provider claim a session (two bdata windows),
  the most recent claim routes invocations. The previous claimant is told it lost the claim and
  can claim again. The claim is visible in the session view ("tools from bdata (window 2)").
* **Offering.** When a turn starts, the daemon builds its tool catalog from plugin tools plus the
  tools of live claimants, intersected with the agent's tool selection, `[agent.<id>.tools]` and
  the turn's `tool_allowlist`. The offered set is recorded in the turn's tool catalog snapshot and
  environment snapshot, as for plugin tools. Tools that appear or disappear in the middle of a turn
  take effect on the next turn, so a running turn's catalog is immutable.
* **No live provider.** If no claimant is connected, the tools are not offered. If a model calls
  one anyway, for example copying an earlier call from history, the result is the tool error
  `client_tool_unavailable` ("bdata is not connected"). History is unaffected.

### Invocation

1. **Authorization first.** The model's call is evaluated by the shared policy over the declared
   `ToolPolicyOperation` and canonical facts. The user's exact-name rules apply, for example
   `[agent.build.permission.tools] "bdata.run_query" = "ask"`. If the policy asks, a canonical
   `PermissionRequested` is shown to every observer, and any authorized client may answer, whether
   the app, `bcode tui` or another app. Remembered decisions go to the daemon's runtime permission
   state. No invocation is sent before the decision completes (`Authorization precedes side
   effects`).
2. **Recording.** `ToolCallRequested` records the producer as the client provider. The tool events
   gain an optional `producer_client_id` beside `producer_plugin_id`, and `ToolSource` gains
   `Client { provider_id }`.
3. **Dispatch.** The daemon sends `invoke { invocation_id, session_id, turn_id, tool_call_id,
   tool_name, arguments_json, deadline }` to the routing claimant. The invocation holds a typed
   ownership guard, like a plugin invocation, so the session stays owned until the result is
   persisted.
4. **Result.** The app answers `result { invocation_id, outcome }`, where the outcome is
   `Completed { content }` or `Failed { code, message }`. Content passes the existing tool-result
   safety bounds and is treated as untrusted (`Tool results are untrusted input`). Duplicate
   results for one `invocation_id` are idempotent when identical and rejected when they conflict.
5. **Cancellation.** Cancelling a turn sends `cancel { invocation_id }`. A result that arrives
   after the terminal outcome is dropped (`Terminal outcomes are stable`).
6. **Deadline.** When `max_duration_ms` passes, the tool result is `client_tool_timeout` and a
   cancel is sent.

Inside the tool, the app may ask its own domain questions in its own UI, such as bdata's "share
result rows with the model?". Observers see the call as running meanwhile. Sharing these
questions with other clients through plugin interactions is a later extension.

### Disconnects and daemon loss

* **App disconnect during an invocation.** The result is `client_tool_disconnected`, with "effects
  unknown" in the message. The call is never re-sent on reconnect.
* **Daemon loss.** The replacement daemon's recovery turns the unfinished call into an explicit
  interrupted result, as it does for plugin tools (`Daemon processes are disposable`). The bridge
  reconnects, the app registers its tools again and re-claims its sessions. Neither registrations
  nor claims are replayed automatically, because the app has to confirm its own live state.

### Permissions in apps

Apps stop evaluating Bcode permissions themselves. bdata's `ToolPolicy`, and its writes to Bcode's
permission state, go away. The daemon's policy covers app tools and plugin tools alike. Product
rules that are not tool permissions, such as bdata's run-queries and share-results policies, stay
inside the app's tool implementations as domain rules.

## App-provided agent profiles and turn context

* **Profiles (decision D5).** A registration may include agent profiles, such as `bdata.sql`, with
  a display name, bounded instructions (32 KiB), a default tool selection (which may include the
  provider's tools and named plugin tools) and a model or effort preference.
  * Contributed profiles may restrict tools but may not grant `allow` rules. Users loosen them in
    their own configuration (`[agent."bdata.sql"]`), so an app cannot give itself permissions.
  * A profile is offered while its provider is connected. If a session's selected profile is
    unavailable, submitting fails closed with `agent_profile_unavailable` and the picker offers
    another agent. The turn environment snapshot records the instructions that were actually used.
* **Turn context (decision D6).** `submit` takes bounded context blocks (16 KiB in total), such as
  bdata's screen summary or the review dock's cursor place.
  * By default the blocks are durable and stored with the user message as context. Feeds show the
    text without them, as the review dock already does.
  * A block marked request-only goes through the request-only context capability
    (`request_context_id`). A queued turn whose request-only context did not survive a daemon
    restart fails closed instead of running without it.

## Live observation

The daemon already supports many attached clients and bounded semantic observation. The bridge
exposes it unchanged. An app and `bcode tui` can watch the same turn stream, see the same pending
permission and either can answer it; the first answer wins and the others see it resolved. A
`client_count` above one is shown in the session view. Observation is state transfer, not durable
resume (`State transfer does not imply durable resume`): after a gap, the app opens a fresh
observation.

## Migrating app-owned JSON stores

bdata's files are Bcode's own `PersistedSession` format, so the importer is generic and belongs to
Bcode rather than bdata (decision D7).

* **Importer.** A bundled, disableable `sdk-session-import` plugin implements the existing
  session-import contract for `PersistedSession` JSON (schema 1 and unversioned). Its source id is
  `sdk-json`. Unlike the Pi and OpenCode importers it has no default roots: callers name files
  explicitly (`bcode session import open --source sdk-json <file> --origin <app_id>`, or the
  bridge's `import_sdk_session`).
* **Result.** Each file becomes a new canonical session with import provenance (`source_id =
  "sdk-json"`, `external_session_id` = the file's session id) and the requested origin.
  * Messages, including tool calls and results, become inert history; tools are never replayed.
  * Persisted memory records are imported as the messages they annotate, with a warning that their
    memory metadata is not carried over.
* **Idempotent.** Importing the same external session id again returns the existing session. A
  canonical session that already claims the same id with different content is reported as a
  conflict and nothing is merged (`Canonical history is never silently merged`).
* **Explicit.** Import runs only when asked, never during listing or open. Each file is a separate
  bounded operation. A failure leaves the source file untouched and is reported for that file.
* **bdata's side** (follow-up work in bdata):
  * When attached mode first starts, bdata offers to import `assistant/conversations/*.json`. It
    shows progress and moves each imported file to `assistant/migrated/<stamp>/`. Files in
    `assistant/deleted/` are skipped unless restored first.
  * Purging `migrated/` is a separate user action.
  * Inline-edit scratch files stay in SDK-local mode or become `Background` sessions, depending on
    decision D8.
* **Rollback.** Until `migrated/` is purged, SDK-local mode can still read the original files.

## Compatibility

* **Unchanged.** SDK APIs (`AgentSession`, `SessionPersistenceAdapter`, `LocalSessionStore`,
  `configured_embedded_*`) are unchanged.
* **Daemon IPC.** The internal daemon protocol gains requests for origin, client tools and claims,
  with a protocol version bump. It stays exact-match.
* **Bridge.** The bridge protocol has its own version and compatibility window, as described
  above.
* **Session storage.**
  * `SessionOriginRecorded` and the client-producer fields are appended at the end of their enums,
    with an event schema bump.
  * Whether the writer epoch must also change is decided by the writer-contract rule: it changes if
    an older writer could no longer maintain required projections. An older daemon fails closed on
    sessions with the newer schema, which is existing behavior.
  * Sessions without origin keep working and are listed as native.
* **Review app.** It adds `origin = bcode.code-review` (context: the review's annotation scope) to
  sessions it creates. `review_assistant_sessions` stays, because it records the revisions a
  session was bound to. Finding the newest session for a review may move to an origin query
  later.
* **Portable contracts.** `SessionOriginSummary` is portable display data in the session models,
  with no terminal or IPC types (`Frontend contracts remain portable`).

## Failure modes

| Situation | Behavior |
| --- | --- |
| No installed Bcode | The app reports "Bcode is not installed" and may offer SDK-local mode explicitly. It does not substitute a location. |
| Installed Bcode outside the bridge window | `hello` fails with the versions on both sides and which one to update. |
| Daemon cannot start | The bridge returns the lifecycle error (log path, exit status). The app shows it. |
| Daemon restarts during a turn | The bridge reconnects, and the app re-registers and re-claims. The unfinished app tool call becomes an interrupted result and is not replayed. |
| App quits during a turn | The turn continues. Calls to the app's tools fail with `client_tool_disconnected` or `client_tool_unavailable`, and other observers keep watching. |
| Two app instances claim a session | The latest claim routes. The other instance is notified and can re-claim. |
| Tool name collision | The registration is rejected as a whole, with the conflicting names. |
| Oversized tool result | Bounded by tool-result safety, with an explicit truncation marker. |
| Profile unavailable on resume | Submitting fails closed with `agent_profile_unavailable`, and the user picks another agent. |
| Catalog cache missing origin | The session is listed as native. Origin filters report it as unclassified and do not hide it from unfiltered listing. |
| Session leased by another artifact (app-hosted runtime) | It is listed. Opening it for changes reports the owner; history reads stay bounded and non-mutating. |
| Import conflict or corrupt file | That file is reported and left in place. Other files continue. |
| App declares a wrong policy operation | The user's exact-name rules override it. The registration is attributed to the app in permission prompts. Apps run as the same user and are trusted like plugins the user installed. |

## Invariants review

The design is checked against `INVARIANTS.md`. It satisfies these without changes:

* `Clients use defined application boundaries`: apps use the bridge or `BcodeClient`, never
  storage.
* `Daemon artifact versions are isolated`: the bridge is the daemon's own artifact.
* `A session has one canonical storage path`, `Aggregated session discovery does not confer
  authority`, `Normal session reads are bounded and non-mutating`, `Damage is surfaced rather than
  concealed`: origin is creation-prefix data with cache-only copies, there is no backfill, and
  unreadable origin is surfaced.
* `Plugin hosts provide plumbing rather than product behavior` and `The runtime remains
  domain-generic`: client tool routing is generic plumbing, and tool behavior stays in the app.
* `Authorization precedes side effects`, `Permission decisions use canonical operation facts`,
  `Tool results are untrusted input`, `Cancellation is end-to-end`, `Terminal outcomes are
  stable`.
* `Persistence is explicit`: context blocks declare durable or request-only.
* `Sensitive ambiguity fails closed`: unavailable tools, profiles and protocol versions fail
  closed.

No change to `INVARIANTS.md` is required by this specification. When the work is built, the
`add-invariant` skill should evaluate two candidates that the implementation can test:

* "Session origin is descriptive: it never selects storage, routes execution or grants
  authority."
* "Client-provided capabilities fail closed when their provider is absent, without changing
  session history."

## Implementation order

Each step is independently useful, keeps the worktree green and lands with behavior tests.

1. **Session origin.**
   * Models, the canonical event, creation through `CreateSessionRequest.origin`, the summary,
     catalog and manifest caches, the origin filter, the picker chip and `bcode session list
     --origin`.
   * The review app tags its sessions.
   * Tests:
     * origin survives restart and catalog loss;
     * an unreadable origin is unclassified and not hidden;
     * filters page correctly;
     * `Background` origin sessions are hidden from pickers but can be opened.
2. **App-provided tools in the daemon.**
   * IPC registration, claims, turn catalog inclusion, dispatch, results, cancellation, deadlines,
     disconnect and recovery.
   * `ToolSource::Client`, generic presentation, and `BcodeClient` support with typed tools.
   * Tests drive a fake app connection through the fake provider:
     * allow, ask and deny;
     * disconnect mid-call;
     * a daemon restart produces an interrupted result with no replay;
     * an unclaimed session never sees the tools;
     * collisions are rejected.
3. **The session bridge.**
   * `packages/session-bridge/models`, `packages/session-bridge` and `bcode session-bridge
     --stdio`, with version negotiation, the stable view projection and every operation listed
     above.
   * Tests run a client built at one bridge version against a bridge that supports a window,
     including an unknown view item that degrades to `other`.
4. **App profiles and turn context.**
   * Contributed profiles (restrict-only), `agent_profile_unavailable`, and durable and
     request-only context blocks.
5. **SDK JSON import.**
   * The `sdk-session-import` plugin, the CLI and bridge operations, idempotency and conflict
     reporting.
6. **Adoption.**
   * The review app uses origin queries for its session lookup.
   * bdata's migration to attached mode (bridge client, tool registration, import, removal of its
     own permission engine) is follow-up work in the bdata repository.

The app-hosted runtime (D2) is not scheduled. Revisit it if an app needs Bcode sessions on
machines without Bcode installed.

## Decisions for sign-off

* **D1, bridge.** Out-of-tree apps connect through a versioned `bcode session-bridge --stdio`
  subprocess of the installed Bcode. The alternatives were a multi-version daemon protocol, which
  would weaken exact-match recovery, and requiring apps to pin the installed Bcode's exact
  revision, which is impractical. In-tree apps keep `BcodeClient`.
* **D2, app-hosted runtime.** Deferred. If it is built, it must be non-migrating in the shared
  location.
* **D3, origin.** A new canonical event in the creation prefix, immutable, secret-free, with no
  authority and no backfill.
* **D4, tool scope.** App tools are offered only in sessions a live connection of the provider
  has claimed, and are fixed for the duration of a turn.
* **D5, profiles.** Contributed profiles may restrict but never grant permissions, and fail closed
  when unavailable.
* **D6, context.** Turn context blocks are durable by default and request-only on request.
* **D7, import.** A generic `sdk-json` import plugin in Bcode. Apps name files explicitly.
* **D8, scratch sessions.** bdata's inline edits stay SDK-local (recommended) rather than becoming
  `Background` sessions.
