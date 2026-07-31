# Daemon lifecycle architecture

## Scope and ownership

Bcode daemon availability is split across explicit owners:

* Artifact production embeds one exact `ArtifactId` in each produced `bcode` executable.
* IPC owns the portable artifact identity contract, protocol version, `Hello`, status payloads, and endpoint derivation.
* Daemon lifecycle owns executable integrity evidence, image materialization, startup locking, spawning, readiness, records, stale endpoint handling, and conservative cleanup.
* The client owns connection and auto-start policy.
* The server owns application initialization and full readiness.
* Frontends observe client outcomes; normal TUI operation does not host a daemon.

The embedded plugin-surface server is a separate explicit integration mode. It does not publish a normal daemon record and is not an availability fallback.

## Identity model

The exact produced-artifact identity is distinct from:

* source/build fingerprint;
* IPC protocol version;
* session event schema version;
* storage writer epoch;
* executable SHA-256;
* daemon process instance identity.

Default namespaces and endpoints derive from protocol version plus exact artifact ID. Endpoint discovery is O(1) and does not inspect executable bytes. `Hello` advertises the intended artifact ID, and both peers reject a mismatch. Build fingerprints and compatibility epochs remain independently checked rather than serving as artifact identity.

Executable SHA-256 remains cold-path integrity and process evidence. It is not a routing key.

## Connection policy

The canonical auto-start path is:

1. Attempt one local transport connection and verified `Hello` using the connection deadline.
2. Return immediately on success or on an incompatibility/application error.
3. If transport is unavailable and auto-start is enabled, invoke one lifecycle ensure operation using the startup deadline.
4. After lifecycle reports readiness, attempt one final verified connection.

Application requests are issued only after a connection exists. The client does not replay a request merely because connection or startup failed. `RequireRunning` and explicit endpoint clients do not implicitly start a daemon.

## Image materialization

Daemon images are stored under an artifact-scoped directory and a SHA-256 subdirectory. The executable is accompanied by versioned metadata containing the exact artifact ID and digest.

Bootstrap captures an open read handle before CLI setup. Cold copy and digest operations rewind and read that retained handle rather than reopening the diagnostic pathname. On Unix this keeps the original inode readable after rename or replacement; on Windows the open handle retains the exact opened file and replacement follows native sharing rules. The source pathname is never trusted as later artifact authority.

Materialization occurs only on the cold start path while lifecycle owns the artifact startup lock. Publication writes a process-specific temporary executable, preserves executable permissions, verifies SHA-256, atomically renames the image, and then publishes matching metadata. Existing images are reused only when both bytes and metadata validate. A corrupt image or metadata pair is removed and rebuilt once while the startup lock is held.

Digest and metadata are retained for diagnostics and conservative process verification. Cleanup retains images referenced by daemon records and the current image, and does not remove ambiguous live historical state.

## Startup coordination and readiness

The startup lock is artifact-scoped through the namespace. A lock holder rechecks readiness before spawn, owns stale endpoint recovery, materializes the image, spawns the child, and waits for readiness. Other processes wait for the lock and recheck rather than intentionally launching another child.

A successful verified `Hello` is full application readiness. The server initializes configuration, plugins, session services, application state, workflow recovery, and ownership behavior before accepting that handshake. No partial-ready protocol is currently justified by measurements.

Lifecycle must preserve a responsive foreign endpoint and report incompatibility rather than deleting it. Endpoint and record cleanup is permitted only with positive stale evidence.

## Timeouts and errors

Connection, startup, and application-request deadlines are separate client settings. Startup errors preserve child exit, lifecycle, coordination timeout, log path, and recent-log context. Frontends may present those errors differently but do not replace them with alternate process ownership.

## Session persistence boundary

Artifact-specific daemon routing does not create artifact-specific canonical session history. All artifacts use the canonical session root. Session leases include daemon compatibility and instance metadata and prevent conflicting runtime ownership across artifact versions. Historical daemon records remain conservative control evidence.

Snapshots, event envelopes, and bounded reconnect checkpoints are state transfer only. They are not described as durable resume unless retention, acknowledgement, replay, and conflict behavior are defined by the relevant transport contract.

## Transition behavior

Records without exact artifact identity are historical records. They may be classified through exact endpoint or process evidence, but they are not treated as current-artifact routing authority. Workaround-era content-addressed images may remain until no live record retains them; new startup publishes artifact-scoped image metadata. Old responsive daemons are not replaced merely because their namespace or protocol is historical.

## Performance boundary

The warm path performs endpoint derivation and one verified local handshake, with no executable file read. Cold-path copy, hashing, and metadata verification are intentionally excluded from warm routing. Evidence and locked budgets are recorded in `docs/daemon-startup-performance.md`.
