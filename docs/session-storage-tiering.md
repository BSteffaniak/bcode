# Session storage tiering

## Outcome and implementation status

The intended outcome is transparent, lossless, default-enabled storage tiering: light compression
at five days without meaningful access, deep compression at thirty days, configurable thresholds
and an opt-out. It is not model-context compaction, retention, summarization, or deletion.

**Automatic compression is not implemented yet.** The first implemented slice is explicit bounded
physical measurement through `bcode session storage-usage <session-id> [--entry-budget 10000]`.
The command returns JSON through the client/application/session boundaries. It measures database
files and engine sidecars, session artifacts, and other session-local files separately. It reports
file lengths and allocated bytes on Unix, skipped entries, and entry-budget exhaustion. It neither
opens the database nor validates canonical history. Corrupt database bytes remain untouched.

The measurement is an observation, not a transactional snapshot: files may change while traversed.
Hard links count per directory entry. Symlinks and special files are excluded; symlinked session and
artifact roots are rejected. Traversal depth and directory entries are bounded. A partial result
must not be presented as a complete total. Database totals include projections and cannot be
interpreted as canonical event payload size. Global catalog and provider-owned search indexes are
excluded, not treated as zero. No compression ratio or space saving is inferred from file lengths.

## Implemented codec foundation (not activated)

The session domain now contains an explicit compressed-artifact codec. Version 1 uses 256 KiB
independent Zstd frames, levels 1 (light) and 12 (deep), a 64-byte checked header, and fixed 80-byte
index entries. Header fields identify the version, chunk size, logical length, and chunk count.
Each entry records physical position/length, expanded SHA-256, and an ordinal-bound entry checksum.
These checks detect damage; they are not authentication against an attacker who can rewrite files.
Levels are provisional until representative benchmarks establish the policy.

Range reads inspect only intersecting entries and frames, enforce the existing 1 MiB request limit,
limit decoder windows to 256 KiB, and reject invalid checksums, expansion lengths, arithmetic,
unsupported versions, and truncated data. Full-container verification is intentionally separate
from bounded reading. The encoder processes one chunk at a time, checks cancellation between chunks,
requires empty output and an exact source length, and writes the checked header only at completion.
It does not publish, sync, replace, migrate, or auto-detect any artifact. An unsuccessful encode is
incomplete output, never authoritative content. No existing artifact reader or writer uses the codec
yet; a durable representation selector and compatibility fencing must precede activation.

An explicit maintenance verifier now compares every decoded chunk directly against the original
stream with bounded memory and cancellation between chunks. It additionally requires contiguous,
ordered physical chunk ranges and exact physical/logical lengths, rejecting overlaps, gaps, trailing
bytes, or a changed original. Range reads remain independent of this full traversal. Verification
does not itself publish data or establish ownership: maintenance must keep both sources immutable
through publication and validate any authoritative original checksum separately.

Candidate preparation combines encoding, total-length savings checks, and whole-original
verification. A candidate is ready only when it is strictly smaller and meets a caller-supplied
absolute savings threshold including all container overhead. Empty/tiny or insufficiently
compressible inputs are explicitly rejected without modifying the original. Cancellation and
verification failures never publish a candidate. The result describes byte lengths, not allocated
disk savings; atomic replacement, syncing, and compatibility fencing are still not implemented.

The logical artifact reader now streams and seeks over either explicitly selected raw storage or
chunked Zstd, retaining only one 256 KiB decoded buffer. Maintenance can prepare light-to-deep
candidates directly from compressed storage without a full-size decompressed temporary, and can
restore original bytes through a cancellable bounded stream. Transition savings are checked against
the existing **physical** representation, not logical content length; repeating the same encoding
does not falsely report the raw/compressed ratio as additional savings. Verified preparation remains
separate from publication. Tests exercise raw-to-light-to-deep-to-raw byte identity, chunk-boundary
seeks, duplicate-tier rejection, explicit format selection, and cancellation.

## Configured policy (not activated)

The configuration loader accepts and validates:

```toml
[session_storage]
enabled = true
light_after_days = 5
deep_after_days = 30
maintenance_interval_secs = 60
artifact_timeout_secs = 30
minimum_saved_bytes = 4096
```

These are the defaults. Thresholds require `0 < light_after_days < deep_after_days`, including when
disabled; unknown fields and invalid values are rejected. Days mean elapsed 24-hour durations.
The settings are documented by the configuration schema but **do not yet start maintenance**.
`enabled` controls scheduling only: meaningful access tracking continues while disabled so later
re-enablement cannot use timestamps that omit reads during the opt-out. Existing compressed data
must remain readable independently of scheduling. This is not a switch disabling metadata writes.

The session domain provides a pure, separately validated eligibility policy. It proposes only colder
representations; it never promotes data after a read. Eligibility uses the later of finalization
and last meaningful access, with inclusive thresholds. Missing tracking, clock rollback, incomplete
artifacts, and ownership not verifiably released all defer. These facts are supplied by a scheduler;
the decision is not authorization and must be rechecked under durable maintenance ownership.
Access tracking integration and the scheduler/config adapter remain to be implemented.

### Access-record persistence and initial read integration

`storage_access` implements bounded observation and durable updates on caller-supplied confined
file handles. The fixed 64-byte record carries a magic identifier, version, timestamp, monotonically
increasing generation, and SHA-256 checksum. Empty tracking is unknown, not old. The API takes
nonblocking shared/exclusive OS file locks and merges stale deliveries against the maximum timestamp.
Duplicate timestamps and clock rollback never reduce that timestamp or rewrite the record. Changed
records are synced before success. Damage, future versions, contention, and generation overflow
return errors; damaged bytes are never automatically reset. Interrupted writes may require explicit
maintenance rather than falling back to an older timestamp that could incorrectly permit tiering.

Meaningful access includes explicit history navigation/inspection/attach/export, original artifact
reads, and model-context consumption. Catalog, indexing, and maintenance scans are excluded.
The low-level handle API remains path-neutral. A session-owned adapter now creates
`<session-id>/storage-access.bin` only when canonical storage already exists. Unix traversal uses
relative directory descriptors and no-follow opens; tracking hard links and nonregular files are
rejected. The record and directory entry are synced. Other platforms currently report unsupported
tracking without creating metadata.

Explicit application history page/window/inspection/export and artifact range reads register
successful consumption. Full, recent, and projection-window attach paths now also register history
access, and model request construction registers model-context consumption. Shared low-level
history reads used by indexing and background invariant selection are unchanged. Registration
currently waits
for a bounded metadata operation. Session timestamps round **forward** to one-minute boundaries,
coalescing record writes within each window without understating access age. The delay to compression
is less than one minute; saturation at the maximum timestamp remains conservative. Each read still
opens and locks the record and syncs the directory; this is not an asynchronous I/O optimization.
Optional tracking failures emit a secret-safe
warning and do not fail the successful content read; **automatic tiering remains disabled globally**,
so these failures cannot authorize compression. Before maintenance can use this state, registration
and maintenance must coordinate so pending/failed writes, untracked old clients, and the
read-to-registration race cannot authorize compression based on stale age. Durable degraded-state
handling remains required before activating scheduling.

## Physical publication and transparent reads

An explicit offline session-domain operation now publishes compressed artifacts on macOS/Linux.
It acquires session maintenance ownership, prepares and verifies a candidate, syncs its payload and
directory, and atomically exchanges it with the original using the platform's exchange-rename API.
The unchanged logical path is always authoritative. A compressed representation is a directory
containing exactly `content.v1.zstd`; unknown contents fail closed. Older raw-file readers fail on
that directory rather than returning compressed bytes. This does not guarantee seamless use by old
binaries and is not a substitute for the remaining compatibility/migration work.

The server's finalized artifact range reader now understands these containers and returns unchanged
logical offsets and bytes. Live artifact paths remain raw. Caller-supplied finalization and canonical
membership are preconditions of the maintenance API; it is not yet exposed as an automatic worker
or CLI command. Unknown pending directories prevent another conversion and are preserved. Before
exchange, cancellation leaves raw authority intact; after exchange, the logical path remains
committed and old content is disposable cleanup. Tests cover actual file publication, server range
parity, active-owner refusal, cancellation, staging residue, and unknown container versions.

Process-crash tests now terminate a separate process without unwinding after candidate sync,
immediately after exchange, and after parent-directory sync. Every case preserves byte-exact reads
at the one logical path, releases the dead process's maintenance lock, and retains staging residue
without treating it as fallback authority. These tests cover process death, not power-loss durability.

This implementation still requires security review of path races against untrusted concurrent
filesystem mutation and stronger finalization/ownership capabilities before enabling automatic
scheduling. It does not compress canonical session databases.

## Verified finalized-reference maintenance

`compress_finalized_artifact` acquires maintenance ownership before resolving the reference through
current session storage. Unsupported contracts, stale projections, absent/incomplete references,
and logical-length mismatches prevent publication. It closes the database before file conversion
and transfers the maintenance guard into the blocking task so cancellation cannot release the fence
while conversion runs. Invocation capability URIs, relative paths, and historical local file paths
now use one session-owned resolver shared with server reads, with filesystem confinement enforced
before maintenance. Malformed capability fields and unsupported schemes are rejected. A database
integration test verifies actual publication and logical range reads for a capability reference.
The lower-level path-based operation still
exists for callers that separately establish finalization and is not safe to schedule blindly.

## Automatic age eligibility

Automatic maintenance now checks both durable last access and the canonical finalizing event's
`created_at_ms` under its maintenance fence. A newly finalized artifact cannot inherit an old
session access timestamp and compress immediately. Missing, malformed, or future finalization
metadata defers rather than guessing an age. An integration regression verifies that an old access
record with a young finalization leaves the raw artifact unchanged.

## Scheduler paging

The experimental worker now processes one reference page per minute and retains an exclusive key
cursor across ticks. Pending sessions from each bounded directory batch are rotated, so a large
session no longer drains its entire reference set in a single tick. The queue retains at most the
sixteen sessions discovered in that batch. Each resumed page reevaluates configuration and access
age; disabled, recently read, ambiguous, or unavailable sessions leave the queue until rediscovery.
This bounds candidate count per tick, not elapsed I/O time or individual artifact size. The worker
is still not activated at daemon startup: durable access-failure fencing and publication path-race
hardening remain unresolved.

## Descriptor-relative publication

Atomic exchange now uses `renameatx_np`/`renameat2` with single-component names and a pinned parent
directory descriptor, not absolute pathname traversal. Before exchange it verifies the parent path
still names the pinned device/inode. A replacement-parent test proves that neither the replacement
nor original files are exchanged on detected substitution. Parent syncing after publication uses
the pinned handle. Staging creation, payload creation, and cleanup now use descriptor-relative
operations too. Publication verifies source, staging, and candidate descriptor identities. Cleanup
checks the prior object's identity and exact known payload before unlinking; substitution or unknown
contents are retained instead of removed. A regression test swaps the staging pathname and verifies
that replacement content survives. Concurrent same-user mutation between identity checks and
syscalls still requires explicit threat-model review; this is not a claim of adversarial atomic
compare-and-swap on inodes.

## Scheduler integration coverage

Clock-controlled tests now invoke the real maintenance pass over canonical finalized session
artifacts, rather than only testing codecs. They verify hot-data deferral, light-tier publication,
continuation from reference 16 through 18, byte-exact range reads, scheduling opt-out, live-owner
refusal, recent-access deferral, corrupt-tracking preservation, and deep-tier output at 31 days.
The worker remains unstarted in production pending durable access-failure fencing; these passing
pass-level tests do not imply lifecycle integration or automatic startup is complete.

## Durable tracking-health primitive

A tracking-epoch fence now persists dirty state before serving its coordinated reader population,
keeps an exclusive OS lock, and requires explicit healthy/drained completion to mark clean. A failed
access update only needs to set an in-memory failed bit because dirty state is already durable.
Crash/abandoned epochs and unknown formats are not silently reset. Tests include process death:
the OS lock releases but the durable dirty state still refuses a new healthy epoch.

This primitive is **not wired into daemon startup**. It currently models one exclusively coordinated
reader population, not all coexisting daemon versions. Activation requires registration and draining
of every reader participant without making optional maintenance block unrelated daemon operation.
Until that integration exists, the worker remains unstarted; this primitive alone does not prove
that stale timestamps from other daemons are safe.

## Worker cancellation

The scheduled conversion path now supplies a shared cancellation token through verified maintenance
into blocking encode/verify work. Shutdown requests cancellation and then awaits actual completion;
it does not drop the async waiter and mistakenly release maintenance ownership while IO continues.
Cancellation is checked before storage access and between codec chunks. Tests cover cancellation
before touching missing storage, worker shutdown before first poll, and shutdown between ticks.
Normal daemon startup still does not launch the worker: multi-daemon tracking-health coordination
and its lifecycle registration remain incomplete.

## Configurable work allowances

The worker consumes configured maintenance cadence, minimum physical saving, and a cooperative
per-artifact timeout. Timer expiry uses the same cancellation token as shutdown and awaits actual
codec completion before releasing ownership. Timers must be positive; invalid direct configurations
fail closed. The timeout is not a hard syscall deadline: blocking filesystem work and a single codec
chunk must return before cancellation can be observed. These controls do not activate startup by
themselves or substitute for multi-daemon tracking-health registration.

## Existing-session eligibility and lock scheduling

Missing access tracking is now initialized to the maintenance observation time, only after acquiring
idle-session ownership and validating current finalized-reference projection state. Existing valid
records are never refreshed by initialization; corrupt records remain errors. The first scan does
not compress anything with unknown age, but a later scan can do so after the configured delay. A
real-storage test verifies initialization, unchanged next-day metadata, and compression after six
days. Maintenance lock acquisition now runs on a blocking task rather than blocking an async runtime
thread. The acquired guard is transferred through verification and conversion.

## Operation-scoped multi-participant admission

A new operation-scoped admission primitive allows independently opened readers to hold a shared
maintenance gate concurrently while each holds its own participant record. Dirty state is synced
before content access; only successful tracking completion cleans the participant. Maintenance
requires an exclusive gate and a complete clean registry snapshot. Dirty/unknown participants block
maintenance but do not prevent other registered readers. Process-crash tests verify OS locks release
while dirty participant evidence persists. This avoids the earlier whole-daemon exclusive fence as
a prerequisite for concurrent readers.

The primitive remains handle-based: durable confined registry creation, complete bounded participant
enumeration, daemon registration, and coverage of older clients are not implemented. It therefore
is not wired to automatic startup yet and must not be treated as proof that every reader participates.

## Confined admission registry

The operation-scoped admission primitive now has a filesystem adapter on macOS/Linux. It creates
and syncs a versioned registry beneath an existing authorized root, creates participant files while
holding the shared registration gate, and scans participants only under exclusive maintenance
admission. Scans have an explicit entry budget; incomplete scans and unknown names refuse admission.
Descriptor-relative no-follow opens reject symlinks and multiply linked participant files. Tests
cover multiple registry instances, concurrent readers, dirty restart, scan exhaustion, unknown state,
and symlink/hardlink refusal.

The adapter is not registered by daemon startup yet. Participant identity reuse/retirement, bounded
lifetime growth, full read-path admission, and older-client exclusion remain necessary before this
registry can safely authorize the automatic worker. Existing participant records must not be deleted
merely to make an incomplete scan fit its budget.

## Participant retirement and application adapter

Completed clean participants can now be retired under exclusive registry admission. Dirty, active,
unknown, or missing participants are not silently removed. Repeated successful register/retire cycles
leave only the coordinator instead of growing the registry with every operation. The server has an
async registration/completion adapter that performs blocking lock/sync work off executor threads and
preserves dirty evidence on abandonment. Its lifecycle test covers exclusion, clean retirement, and
abandoned-operation refusal. Retirement contention remains an explicit error with a retained clean
record, not unsafe deletion.

The adapter is not yet invoked by production content reads and the worker is still unstarted.
Read-failure fallback, complete admission coverage, safe clean-record cleanup, and old-client
participation remain unresolved. These APIs must not be mistaken for enabled automatic compression.

## Worker lifecycle ownership

The server now has an owned worker lifecycle wrapper with idempotent start, disabled/shutdown
refusal, and cancellation-safe join ownership. Cancelling a shutdown waiter no longer requires
abandoning its task handle; another waiter can finish draining the same worker. Tests cover repeated
start, no restart after shutdown, and a cancelled stop retaining its worker until actual completion.
The wrapper is not invoked by production startup yet: caller-established tracking-health coverage
remains a precondition and has not been integrated.

## Initial production admission wiring

Explicit history export, history pages, around-sequence windows, and inspection now attempt
registered admission before reading and clean their participant only after access persistence
succeeds. Cancellation or failed tracking leaves dirty evidence. Registry failure does not break
canonical reads; the unregistered fallback is explicitly unsafe for automatic scheduling, so startup
activation remains disabled. Artifact, attach, and model-context admission plus a durable fallback
fence still need integration before enabling the worker. This partial wiring is not completed
tracking-health coverage.

## Enforced automatic publication admission

Age-based compression now acquires complete clean registry admission inside the session-domain
mutation operation, not merely in the scheduler. The registry gate remains held through validation
and transfers into the blocking conversion task until publication/cleanup completes. Dirty records,
active registered readers, unknown entries, and over-budget registry scans therefore prevent actual
automatic replacement. Integration tests prove active/abandoned readers leave a raw artifact intact
and successful reader completion permits a later real compression pass. This closes the previously
unenforced-registry gap, but does not establish coverage for unregistered fallbacks or older clients.
Startup activation remains disabled until those read paths are safely coordinated.

## Completion without registry accumulation

Successful registered reads now retire their exact participant while still holding shared admission,
instead of releasing admission and then attempting an exclusive upgrade. Unrelated concurrent reads
therefore no longer cause normal completion to leak clean participant records. Retirement checks
file identity, keeps the participant dirty until unlink plus directory sync completes, and retains
admission throughout. Wrong-identity completion is rejected. A regression exercises one long read
alongside one hundred shorter completed reads and verifies only the long participant remains.

## Startup wiring status

Daemon startup now owns/starts the enabled storage worker and shutdown awaits its completion.
Read-admission and timestamp failures set a process-local failure latch. However, the worker's
compatibility-readiness check currently fails closed unconditionally: the task waits for shutdown
and does not dispatch compression. The `storage.maintenance.compatibility_ready` gauge is zero.
This is lifecycle wiring, **not automatic compression activation**, and the failure latch is not a
replacement for durable cross-daemon fallback registration. Older-client coordination and canonical
history compression are still unimplemented.

## Remaining implementation

### Access policy and scheduling

* Persist application-owned, coalesced usage metadata separately from canonical history. Ordinary
  reads must not rewrite canonical events, perform repair, or trigger format migration.
* History hydration and artifact range reads count as access; catalog listings, compression scans,
  and background indexing do not. Model-context consumption counts as meaningful access.
* Unknown access age must not be guessed from filesystem atime. Initialize tracking conservatively.
* Keep active artifacts and currently owned sessions out of maintenance. Scheduling policy should
  be disableable without disabling readers for already-compressed storage.
* Bound candidate discovery, CPU, memory, I/O, temporary storage, and cancellation latency. Resume
  scans incrementally rather than repeatedly visiting only the earliest sessions.
* Do not rewrite cold data after one read. Serve bounded decompressed data and use sustained access
  for any optional promotion. Compression benchmarks must select codec levels and chunk sizes.

### Artifact representation

Current application range reads consume raw files, including finalized invocation artifacts.
Before replacement, audit all producer, export, import, plugin host, and renderer read paths.
Retain logical references, offsets, lengths, finalization revision, and content checksums. A versioned
chunk container must identify its own authoritative representation; a disposable projection cannot
be the only way to distinguish compressed bytes from raw content. Include integrity and expanded
size checks and a bounded seek index; tiny reads must not decompress entire recordings.

The current finalized-artifact database lookup now validates the storage writer contract before
checking projection freshness or resolving a URI. Missing and non-current contracts fail closed,
without mutating storage. This strengthens the updated reader but does not retrofit older binaries:
it is not sufficient by itself to permit compressed-file replacement.

Artifact reads now retain a session runtime-work ownership guard from before reference resolution
through physical byte consumption and access registration. This closes the idle-release gap between
lookup and file I/O: offline maintenance cannot acquire authority during a successful guarded read.
The async caller retains its guard through access registration, and each blocking file task owns a
clone until physical I/O ends. Cancelling the async caller therefore cannot release ownership while
an uncancellable `spawn_blocking` read is still running. Invalid
range sizes are rejected before acquiring ownership. This does not fix untracked older binaries or
replace the required format fence.

Older daemons must be fenced from formats they cannot read, including finalized artifact paths that
bypass normal canonical-history loading. Implement ownership-verified, interruption-safe replacement
and recovery before enabling any background writer. Never leave both old and new representations
ambiguously authoritative. Skip already compressed or incompressible content when net savings do
not justify the conversion.

### Canonical event payloads

Keep the canonical `session.db` at its existing path, with queryable sequence and metadata indexes.
Do not archive whole databases or shift authority into external event packs or search indexes.
Current event rows contain JSON text; migration readers also interpret JSON directly. A compressed
payload format therefore requires a compatibility-defined storage change, a narrowly scoped
current-format target, and migration-owned historical conversion. Audit every payload consumer,
including original usage, export, repair, and bounded search projection ingestion.

Maintain append-only logical history exactly. Automatic known lossless upgrades require exclusive
verified ownership, incompatible-reader fencing, interruption-safe recovery, and preservation of
unknown future state. Recompression and actual allocated-space reclamation need separately bounded
maintenance designs; smaller values alone do not guarantee smaller database files. Neither normal
reads nor daemon startup may implicitly full-replay history to implement this feature.

### Search and verification

The existing compressed search plugin stores derived normalized records in Zstd chunks; it does not
compress original artifacts or canonical history. Search providers retain ownership of indexes and
coverage policy. Storage temperature must not alter matching, ranking, pagination, or coverage.
Hydrate result locators with bounded canonical reads. Missing/stale search state remains explicit,
not an excuse to silently scan all cold history or warm every matching session.

Extend measurement with provider-owned storage diagnostics before claiming complete session-space
accounting. Benchmark real representative text, recordings, images, sparse files, database free
pages, and search-index overhead. Test byte equivalence, chunk-boundary reads, seeking, search parity,
active-owner exclusion, competing daemon versions, concurrent reads, cancellation, low disk space,
corruption, unsupported versions, and crash recovery. Do not enable automatic transitions before
these safety and compatibility paths are implemented.
