# Daemon lifecycle architecture

## Scope and ownership

Bcode daemon availability is split across explicit owners:

* Artifact production embeds an explicit `ArtifactId` for packaged executables; raw Cargo builds resolve identity from final executable bytes.
* IPC owns the portable artifact identity contract, protocol version, `Hello`, status payloads, and endpoint derivation.
* Daemon lifecycle owns executable integrity evidence, image materialization, startup locking, spawning, readiness, records, stale endpoint handling, and conservative cleanup.
* The client owns connection, auto-start, and transparent replacement-daemon reconnection policy.
* The server owns application initialization and full readiness.
* Frontends observe client outcomes; normal TUI operation does not host a daemon.

The embedded plugin-surface server is a separate explicit integration mode. It does not publish a normal daemon record and is not an availability fallback.

## Agent runtime executable context

The server resolves the lifecycle-owned `PinnedBcodeExecutable` during state initialization.
Its immutable path, executable digest, artifact identity, and serving state root are included
in turn environment snapshots when the runtime-mode section is enabled. Prompt construction
uses the initialized context without rehashing or copying executable bytes per turn.
Paths are complete JSON strings rather than truncated display values; paths that cannot be
represented faithfully and failed image verification produce an explicit unavailable state,
never a PATH-resolved substitute.

Default coding guidance directs agents to use this executable for Bcode CLI operations,
including sub-session control, while preserving state/configuration selection. This is
execution identity plumbing, not a collaboration strategy or a permission grant. Snapshot
retention follows the existing turn-context rules; replacing prompt text remains supported.

## Agent configuration selection context

Initiating clients carry non-secret config launch provenance through `ClientRuntimeContext`:
explicit file layers, whether inline overrides exist, and the user provider-config directory.
The execution host combines that provenance with the actual resolved turn provider/model
and named profiles. It never substitutes daemon environment for missing client provenance
or serializes provider settings, authentication material, or environment maps into this guidance.

This guidance is request-only and controlled by
`[system_prompt.sections] config_selection = false` (enabled by default). It advertises
`BCODE_CONFIG`, context/profile usage, bounded directory discovery, and centralized model
resolution. One file is a reproduction starting point, not proof that unchanged files,
project layers, credentials, or profile settings match: worker selection must be verified.
Multiple layers and inline overrides are explicitly distinguished from file-only reproduction.
No config contents are copied into orchestration documents or durable prompt snapshots.

## Runtime-work observations

The server's runtime-work registry holds execution cancellation and session ownership.
Durable runtime-work rows retain historical observations; a `Running` row alone is not
proof of current execution. Runtime-work subscriptions intersect durable seed rows with
currently registered ownership and use the registry's cancellation status. Unowned rows
remain available through bounded history, but are not seeded as live work. This read
neither repairs history nor infers successful completion, absent effects, or safe replay.
Missing registry ownership is not proof that a foreign or unknown process has exited.

Tool result completion releases execution ownership and attempts a terminal runtime-work
observation even when result persistence fails. Result persistence errors still propagate;
no result or effects are replayed. If terminal persistence also fails, live subscription
seeding still cannot revive the historical row. Existing live subscriptions can miss that
terminal observation on persistence failure; explicit unknown-liveness observations need
an independently compatible public contract rather than fabricated terminal events.

## Session stream continuity and diagnostics

The TUI retains an in-flight IPC receive across progress-flush timers. Framed reads
are not cancellation-safe: abandoning one requires discarding the connection, not
starting a fresh read on the remaining payload bytes. The TUI uses explicit receive
errors to display resynchronization before reconnecting and loading bounded state.

Failed event writes invalidate the connection, including writes interrupted by the
send deadline. Session forwarder exit or cancellation also invalidates its connection;
a live socket must not conceal a dead session subscription. Normal session execution
is independent of this client transport teardown.

Default background/client tracing enables `bcode_server::session_stream` and
`bcode_tui::session_stream` at info level in the namespace-specific daemon log.
Diagnostics include session/client identifiers, event category, retry delay, and
recovery transitions, but not event payloads or raw provider/tool errors. Explicit
`BCODE_LOG` or `RUST_LOG` filters override these defaults. These logs complement
`ipc.event_send.errors_total`; they do not constitute durable stream acknowledgments.

## Request-scoped provider execution

* **`bcode_model::ProviderRequestContext`** owns the complete per-turn provider profile. When that
  context is supplied, provider plugins treat it as authoritative and must not fall back to daemon
  process environment or startup model defaults. The context is snapshotted when work is queued and
  remains transient; credential values are never persisted to session history.
* **Model request-target resolution** owns exact model identity and catalog-resolved API surface.
  Bedrock Responses, Messages, and Converse requests may therefore execute concurrently in one
  daemon without deriving route semantics from endpoint hostnames or daemon startup environment.
* **Recovery** fails closed when abandoned historical work lacks a versioned, secret-free execution
  descriptor. It must not replay a turn using the replacement daemon's provider defaults.

## Identity model

The exact produced-artifact identity is distinct from:

* source/build fingerprint;
* IPC protocol version;
* session event schema version;
* storage writer epoch;
* durable runtime scope identity — the `(state root, config directory)` pair;
* executable SHA-256;
* daemon process instance identity.

Default namespaces derive from protocol version plus exact artifact ID. Default endpoints derive from
protocol version, exact artifact ID, and the identity of the resolved **runtime scope** — the pair
`(state root, config directory)` — so daemons serving different scopes coexist without sharing
endpoints, registries, or coordination state. Endpoint discovery is O(1) after process identity
initialization. Production builds embed an explicit unique identity; raw Cargo builds without an
explicit identity hash their final executable once per process and cache a `cargo-sha256-` identity.
This avoids dependency build-script label reuse after final-target-only relinks; unreadable bytes
fail closed. Raw-build signing or stripping changes identity, whereas explicit production IDs
remain stable through those operations. `Hello` advertises the intended artifact ID and runtime scope identity, and both
peers reject a mismatch. Build fingerprints and compatibility epochs remain independently checked
rather than serving as artifact identity.

The Unix endpoint lives in a private `0700` Bcode directory under `/tmp`, with a
`0600` socket. Both directory and socket names retain the endpoint scope digest;
paths remain within the portable Unix socket limit and do not depend on `TMPDIR`.
Bcode explicitly selects the transport's strict policy: effective-user ownership,
protected ancestors, no immediate-parent symlinks, and same-user peers. Parent
preparation and validation precede stale-socket recovery; insecure existing
parents are rejected rather than repaired. The transport owns secure parent
creation, not an earlier generic `create_dir_all` call. Old flat `/tmp` sockets
are not adopted as a fallback; existing artifact identities remain independent.
Explicit endpoint overrides must meet the same security requirements.

Runtime scope identity is a digest of the canonicalized state root and config directory. Config
directory participates because it selects plugins, permissions, and model policy, so two config
directories must not share a daemon process. Verifying the identity on both sides is required rather
than redundant: endpoint scoping alone can be bypassed with an explicit `BCODE_IPC_ENDPOINT` or
`BCODE_SOCKET` override, and connecting across scopes would let a client mutate canonical session
storage it does not own or apply the wrong permission policy. A peer advertising no scope identity is
unverifiable and refused rather than assumed local. See [State Locations](state-locations.md).

Because a daemon owns exactly one runtime scope, it never opens a session belonging to another scope;
a session outside the current scope is served by starting a separate daemon that owns its own leases
and locks.

Daemon registry records live under the state location they describe and also carry that identity, so a
record copied or inherited from another scope is surfaced rather than silently treated as local.

A spawned daemon receives its state root, session root, and config directory explicitly rather than
inheriting them, so an explicit command-line selection is not lost across the process boundary.

Executable SHA-256 is additionally verified by the client against its retained process bootstrap. The `pinned_bcode_executable` capability owns the immutable launch path, artifact identity, and digest. Cache selection requires the retained executable's digest, not merely a matching artifact label. Subsequent access rejects a missing or modified pinned image.

## Connection policy

The canonical auto-start path is:

1. Attempt one local transport connection and verified `Hello` using the connection deadline.
2. Return immediately on success or on an incompatibility/application error.
3. If transport is unavailable and auto-start is enabled, invoke one lifecycle ensure operation using the startup deadline.
4. After lifecycle reports readiness, attempt one final verified connection.

Application requests are issued only after a connection exists. Initial connection or startup failure does not cause the client to replay an application request. `RequireRunning` and explicit endpoint clients do not implicitly start a daemon.

After a verified connection has been established, ordinary daemon loss is recoverable rather than a terminal frontend outcome. An auto-start client keeps its frontend state, coordinates a matching replacement for the same exact artifact and runtime scope, reconnects with bounded backoff, restores its runtime context, session attachment, and subscriptions, then refreshes from bounded authoritative snapshots. Recovery retries reads and mutations with defined duplicate-delivery semantics; it never blindly replays an operation whose side effects may already have occurred. `RequireRunning` clients report loss without starting a replacement, while explicit callers may reconnect to their configured endpoint without changing its authority.

Daemon replacement does not itself terminalize authoritative nonterminal work. The matching replacement daemon uses bounded current-format projections to classify unfinished work, resumes safe work, and converts unverifiable side-effecting operations into explicit interrupted results before continuing. Recovery remains ownership-fenced, preserves stable terminal outcomes, and never substitutes another artifact or state location.

## Image materialization

Daemon images are stored under an artifact-scoped directory and a SHA-256 subdirectory. The executable is accompanied by versioned metadata containing the exact artifact ID and digest.

Bootstrap captures an open read handle before CLI setup. Cold copy and digest operations rewind and read that retained handle rather than reopening the diagnostic pathname. On Unix this keeps the original inode readable after rename or replacement; on Windows the open handle retains the exact opened file and replacement follows native sharing rules. The source pathname is never trusted as later artifact authority.

Materialization occurs only on the cold start path while lifecycle owns the artifact startup lock. On supported macOS and Linux filesystems, lifecycle first attempts a copy-on-write clone from the retained exact-artifact file descriptor; unsupported filesystems fall back to a fused stream copy and SHA-256 pass. Both paths verify SHA-256, preserve executable permissions, synchronize the temporary image, atomically rename it, and then publish matching metadata. Existing images are reused only when both bytes and metadata validate. A corrupt image or metadata pair is removed and rebuilt once while the startup lock is held.

Digest and metadata are retained for diagnostics and conservative process verification. Cleanup retains images referenced by daemon records and the current image. Clients pin their exact executable after selecting the state location, including when connecting to an existing daemon; the pin holds a shared per-image file lock until process exit. Daemons use the same pin. Lease files live under `daemon-image-leases`, outside removable image directories, and are never unlinked by image cleanup. The kernel releases locks on normal exit or crash.

A state-root image lock coordinates publication and shared lease acquisition against cleanup. Cleanup skips rather than racing startup, and requires a nonblocking exclusive per-image lock through removal. Both routine cleanup and explicit offline pruning respect those locks, so an idle daemon is not evidence that its clients no longer need the image. Unrelated unused lease-managed images remain reclaimable. Missing or unreadable lease evidence preserves images; existing unleased images are never automatically promoted to lease-managed images, because older processes cannot hold the new locks. Such legacy images remain conservatively retained. Older Bcode versions do not honor the new per-image locks and must not be used to prune images still needed by upgraded processes.

Malformed records, unreadable registry evidence, and unknown record schemas also make routine image cleanup fail closed so a newer or historical daemon image is never removed merely because the current build cannot interpret its evidence. This cache lifecycle does not change canonical session storage locations.

## Startup coordination and readiness

The startup lock is artifact-scoped through the namespace. A lock holder rechecks readiness before spawn, owns stale endpoint recovery, materializes the image, spawns the child, and waits for readiness. Other processes wait for the lock and recheck rather than intentionally launching another child.

On Unix, spawn inherits a duplicate of the locked open file description as stdin.
The parent closes rather than explicitly unlocks its copy; the child retains the
lock for its lifetime. Thus launcher cancellation or death before endpoint bind
cannot release coordination while the child is alive. This relies on the daemon
retaining stdin; it must not close, replace, or expose it to plugins. Non-Unix
platforms retain the previous lock behavior and do not yet have this handoff.

Normal detached launch reserves stdout for the private `BCODE_READY_V1\n`
notification, opted into by `BCODE_DAEMON_READY_STDOUT=v1`. Logs remain on stderr.
The launcher reads exactly one bounded token, races child exit and a startup
deadline, then performs the existing verified handshake. Unknown tokens, EOF,
and failed verification fail closed. Notification is not identity evidence.
A departed launcher does not stop the daemon. Concurrent waiters and explicit
in-process startup still use bounded probing; this is not yet a fully event-driven
multi-client readiness protocol.

A successful verified `Hello` is full application readiness. Lifecycle readiness performs that exact handshake and validates artifact identity, protocol, build fingerprint, storage writer epoch, and session event schema without hashing executable bytes. The server initializes configuration, plugins, session services, application state, workflow recovery, and ownership behavior before accepting that handshake. No partial-ready protocol is currently justified by measurements.

Lifecycle must preserve a responsive foreign endpoint and report incompatibility rather than deleting it. Endpoint and record cleanup is permitted only with positive stale evidence.

## Timeouts and errors

Connection, startup, and application-request deadlines are separate client settings. Startup errors preserve child exit, lifecycle, coordination timeout, log path, and recent-log context. Frontends may present those errors differently but do not replace them with alternate process ownership.

## Session persistence boundary

Artifact-specific daemon routing does not create artifact-specific canonical session history. All artifacts use the canonical session root. Session leases include daemon compatibility and instance metadata and prevent conflicting runtime ownership across artifact versions. Historical daemon records remain conservative control evidence.

The canonical workflow store is likewise shared rather than forked by artifact. Active runs persist
the current coordinator artifact and generation-fenced authority. A stale or foreign coordinator
cannot mutate a run. After positive evidence that the prior owner ended and acquisition of canonical
session ownership, a different artifact may atomically acquire recovery-only authority. A durable
barrier preserves the original execution artifact and prohibits dispatch; takeover is not execution
compatibility. Missing owner records remain unverifiable, not proof of termination. Recovery-only
operations retain session ownership and recheck authority. See
[workflow persistence](workflow-persistence-architecture.md#reconciliation-and-repair) for the
implemented boundaries and remaining reconciliation gaps.

Snapshots, event envelopes, and bounded reconnect checkpoints are state transfer only. They are not described as durable resume unless retention, acknowledgement, replay, and conflict behavior are defined by the relevant transport contract.

## Transition behavior

Records without the process's exact artifact identity are never contacted, including for liveness probes. Process evidence may prove termination; otherwise foreign ownership remains unverifiable. Maintenance commands do not launch a historical executable to control its daemon. Use a separately started matching Bcode process for that operation. Identity-mismatched or unverifiable records are preserved and refused rather than guessed.

## Performance boundary

The first verified connection hashes the retained bootstrap executable; subsequent identity checks reuse that process-pinned digest. Launches verify the immutable image, and foreign registry targets are rejected before transport access. Historical startup performance budgets must be remeasured for these stronger integrity checks.

## Packaged helpers

Mermaid rendering uses BMUX's embedded WASM renderer. Portable guest WASM and destination AOT artifacts are bundled at compile time; bootstrap does not discover or snapshot an external renderer executable. Renderer initialization failures disable Mermaid rendering only.
