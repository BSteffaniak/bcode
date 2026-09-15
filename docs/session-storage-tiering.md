# Session storage tiering

## Outcome and implementation status

The intended outcome is transparent, lossless, default-enabled storage tiering: light compression
at five days without meaningful access, deep compression at thirty days, configurable thresholds
and an opt-out. It is not model-context compaction, retention, summarization, or deletion.

**Automatic dispatch is implemented for the coordinated clean-break rollout.** Startup requires a
healthy current-daemon registration and no local tracking-failure latch; every operation still
requires durable admission and verified maintenance ownership. The worker initializes unknown
access ages conservatively rather than immediately compressing existing sessions. Historical
entries below describing an unconditional disabled gate are superseded by this status.

The real-file worker integration test now exercises age-based artifact and canonical-history
compression, more than one MiB of actual database reclamation, transparent artifact reads, exact
history preservation after reopening, and continued canonical writes. It runs the production
scheduling loop with a supplied clock; production uses wall-clock time. This supplements the
component migration, fallback-admission, compression, and reclamation tests. Process-crash recovery
and complete rollout-path migration coverage still require separate verification.

The first implemented slice was explicit bounded
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

## Approved clean-break rollout

Automatic session-storage compression targets a coordinated clean-break deployment. Before activation at a state location, the operator stops all older Bcode clients and daemons accessing that location and upgrades every participant. Mixed-version access to that location after activation is unsupported. This deployment prerequisite is not inferred from clean registry files.

Legacy-reader coexistence and historical-client exclusion tests are no longer activation prerequisites for this rollout. The previously proposed `session.db` directory transition is not required solely to fence older readers; the current regular-file layout remains authoritative.

Current-version safety requirements remain: verified exclusive maintenance ownership, active-session exclusion, durable read/access coordination, failure handling that cannot authorize compression from stale evidence, bounded scheduling, cancellation, and interruption-safe publication and recovery. Existing canonical data must remain losslessly readable; unknown or inconsistent state must be preserved and surfaced rather than guessed.

Automatic dispatch remains disabled until these requirements and the complete scheduling path are validated. A deployment upgrade does not justify bypassing runtime safety checks or merely replacing the readiness gate with `true`.

Earlier implementation notes and the remaining-work discussion below describe historical compatibility blockers. Their requirements for legacy-reader fencing and a database-directory transition are superseded by this decision; their current-version safety requirements still apply.

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

## Original artifact checksum validation

Verified-reference compression now validates any projected SHA-256 checksum against the original
logical byte stream before candidate preparation. Malformed checksums or differing bytes reject the
operation; compressed-container self-checks alone no longer substitute for supplied original evidence.
Validation is streaming and checks cancellation between bounded reads. Missing optional checksums
still rely on byte-for-byte candidate verification and established finalization/ownership. Concurrent
untrusted replacement between this check and the later source open remains part of publication
identity hardening, not something a checksum alone resolves.

## Event-payload codec foundation

A versioned, checksummed Zstd/base64 event-payload envelope now preserves exact logical JSON bytes,
including private usage evidence and unknown JSON fields. Expansion, decoder-window, encoded-size,
UTF-8 and checksum checks bound decoding. Incompressible/tiny values remain raw JSON. Current event
and private-usage decoding accept the envelope, but canonical writes still emit plain JSON.
No migration, history recompression writer, or database-space reclamation is activated. Enabling
writes requires the storage epoch upgrade and completion of every remaining direct payload consumer;
this codec foundation alone is not history compression functionality.

## Compressed-history read integration

Canonical row pages and migration-target row pages now decode the storage envelope to exact logical
JSON before returning their typed contract; historical policy does not receive a compressed string
as event JSON. A database integration test stores a compressed fixture payload and verifies bounded
history, around-sequence navigation, and canonical-row JSON equivalence while the stored payload
remains compressed. Production history compression writes and epoch migration are still not enabled.

## Database reclamation backend blocker

An explicit session-owned reclamation operation acquires maintenance ownership, validates the
current writer contract, asks the database backend to VACUUM, closes its connection, and reports main
file lengths. It never copies canonical history or edits engine sidecars. The currently locked Turso
backend rejects this operation with `VACUUM is an experimental feature. Enable with
--experimental-vacuum flag`. Bcode does not implicitly enable that experimental feature.
Tests verify this refusal preserves database bytes and releases maintenance ownership. Therefore
actual database-space reclamation is **not available** through the current backend configuration;
a supported backend capability or explicitly reviewed persistence change is still required.

## Reclaimable capacity accounting

The session database now measures free-page capacity through backend `freelist_count` and
`page_size` statistics, validating the writer contract and checked byte arithmetic. Explicit
reclamation skips VACUUM when no free pages exist. Before/after main-file measurements are taken
while maintenance ownership is still held, avoiding a new writer racing the reported result.
A test creates and frees a MiB of fixture storage and verifies positive reclaimable capacity.
Inspection of the locked dependency confirmed Turso exposes only `experimental_vacuum`, and
Switchy's current database builder does not expose it. Actual compaction remains unsupported by
the configured backend; capacity measurement is not reported as space already reclaimed.

## Reclamation waiter cancellation

Reclamation now runs in an owned task that retains its maintenance guard and database connection
through close even when the caller drops/cancels its waiter. A regression aborts the waiter while
maintenance is held, proves another lease is refused, then permits completion and verifies a new
owner can acquire the session. This is completion safety, not durable resumability or mid-VACUUM
cancellation; the configured backend still refuses experimental VACUUM when free pages exist.

## Expanded production admission coverage

Artifact range reads, full/recent/projection-window attachment, and model request context construction
now attempt registered admission before consuming content, and retire only after the corresponding
access timestamp is persisted. Attachment tracking was moved into the application operation rather
than duplicated in response-formatting helpers. Artifact blocking tasks continue holding session
ownership on cancellation; abandoned admission remains dirty, preventing age-based publication.
Admission failure still falls back to ordinary reads with only a local failure latch, so durable
fallback registration and older-client compatibility remain blockers to enabling dispatch.

## Abandoned artifact-conversion waiters

Dropping the async waiter now requests cancellation of blocking artifact conversion automatically.
The blocking task retains maintenance and registry admission until it returns; completion disarms the
waiter guard so successful work does not poison a caller's shared cancellation token. Tests abort a
waiter while its task holds session maintenance, verify a competing lease remains refused, and then
verify terminal cancellation releases ownership. This closes waiter-abandonment cancellation but
does not enable dispatch or replace missing older-client/fallback compatibility coordination.

## Typed reclamation capability outcomes

Explicit reclamation now distinguishes no free pages, completed engine compaction, and the locked
backend's known disabled-VACUUM refusal. The unsupported outcome includes measured reclaimable
capacity rather than claiming it was reclaimed. Other backend, compatibility, ownership, and IO
failures remain errors. The existing strict operation converts unsupported capability to an explicit
unsupported error. Tests verify positive capacity accompanies the refusal and database bytes remain
unchanged. This does not enable experimental VACUUM or complete actual space reclamation.

## Confined eligibility reads

The scheduler's discovery observation and the session-domain final age check now share a
non-mutating descriptor-relative access-record reader. Missing tracking returns unknown without
creating a file; missing canonical authority, symlinks, hard links, damaged/future records, and
contention fail closed. A regression verifies valid external tracking bytes cannot confer eligibility
through either a symlink or hardlink. This closes an eligibility-path substitution gap, not the
separate cross-daemon unregistered-read fallback gap; dispatch remains gated off.

## Maintenance-engine reclamation activation

Explicit reclamation now uses the same exact Turso 0.6.1 already locked by Switchy, through a
session-private maintenance connection. After current-contract validation and closing the ordinary
connection, that maintenance connection alone enables `experimental_vacuum(true)`. The engine owns
temporary database creation, source transaction/WAL publication, and recovery; Bcode does not copy
canonical rows or edit sidecars. Normal connection configuration is unchanged. Both engine handles
are dropped before releasing the existing maintenance lease. This intentionally uses the engine's
experimental compaction capability only for explicit, exclusively owned maintenance.

Tests now verify real main-file reduction of at least four MiB, canonical history and writer-contract
preservation, reopening through the normal backend, and ownership release. This supersedes the earlier
unsupported-only reclamation status. Automatic scheduling of reclamation and process-crash injection
during the engine's internal VACUUM phases still need integration/validation.

## Writer epoch 10 compatibility boundary

The current session writer epoch is now 10. Migration-owned inventory includes the released epoch-9,
schema-47 combination and the 9-to-10 edge; existing fixture lifecycle matrices exercise the added
source epoch. Older writer-contract validators reject epoch 10 instead of assuming epoch-9 storage
semantics. The event schema remains 47: storage representation compatibility is separate from event
semantics. The existing verified migration coordinator still owns conversion and writer finalization.
No compressed-history writes are enabled by this bump alone; dispatch/fallback coordination and the
history maintenance writer remain incomplete.

## Epoch-10 history maintenance writer

`compress_history_page` now recompresses at most sixteen current canonical events per transaction,
under idle-session maintenance ownership and writer-contract validation. It validates event identity,
compares candidate bytes against exact logical JSON, preserves private fields, updates only smaller
physical payloads, and leaves projections and event sequences unchanged. A continuation cursor allows
bounded subsequent pages; repetition is idempotent. Normal appends remain plain JSON. Tests verify
real compressed writes, logical history/JSON equality, continued appends, and transaction rollback
when a later row in the page is corrupt. Reclamation remains a separate maintenance operation.
This entry point is not yet connected to age-based automatic dispatch; cancellation budgets and
full historical migration round-trip coverage remain necessary for that integration.

## Age-fenced history scheduling phase

The worker now carries typed artifact/history cursors. After artifact pagination, it advances bounded
history pages across later ticks, selects the configured light/deep level, and reclaims database
space after reaching history EOF. The history operation acquires registry admission and maintenance
ownership, rechecks access age, and skips events newer than the cutoff while advancing the cursor.
A test verifies old and recent events coexist without compressing the recent event and that a new
access timestamp prevents later recompression. The global dispatch-readiness gate still returns false;
this connects the internal scheduling phases, not completed production activation or fallback safety.

## Atomic history cancellation

History pages now accept cooperative cancellation checked before the transaction, between events,
and immediately before commit. A cancelled page rolls back all compressed payload changes. The
worker uses the configured per-artifact allowance for each history page and cancels/drains it on
shutdown or timeout. Dropping a waiter requests cancellation while the owned task retains registry
admission and session ownership through transaction completion and database close. A regression
cancels after a payload update but before commit and verifies physical payload bytes are unchanged.
The dispatch gate remains disabled pending durable fallback and older-client coordination.

## Integrated history/reclamation proof

A real-file test now creates forty large canonical messages through SessionManager, compresses them
across bounded maintenance pages, reclaims more than four MiB, reopens through normal session APIs,
verifies exact event history and bounded backward pages, and appends successfully afterward. A
separate test downgrades a fixture's writer contract to epoch 9 and verifies compression refuses it
without altering the logical payload or epoch. These prove the offline history/reclamation path;
they do not bypass the still-disabled dispatch gate or establish durable fallback coordination.

## Reader compatibility differs from writer compatibility

The full session suite confirms that bounded and complete history investigation intentionally remain
available without writer-epoch compatibility or runtime leases. Do not impose writer equality on
these read paths: a readable event representation is sufficient for investigation even when mutation
is refused. A new regression verifies unsupported compressed-envelope versions reject history and
canonical-page reads without rewriting bytes. The epoch-10 writer upgrade therefore cannot by
itself prove that older binaries participate in access tracking; older-reader coordination remains
an explicit unresolved activation requirement, not permission to weaken read availability.

## Durable fallback blocker

Admission failure now attempts an idempotent durable `unregistered-read.blocked` marker under the
shared registry gate before returning the ordinary-read fallback. Complete maintenance registry
scans reject this marker after restart; successful subsequent reads never clear it. Tests verify
persistence, idempotency, ordinary-reader availability, and refusal while maintenance holds the gate.
If even this marker cannot be persisted, the implementation still returns the ordinary-read fallback
with a local failure latch; dispatch must therefore remain disabled until that last fallback case
has a pre-established durable fence. This marker improves one path but does not complete fallback
safety or older-client coordination.

## Startup daemon fallback registration

Startup now durably registers a daemon-specific ACTIVE record in the admission registry before
launcher readiness/content requests. Registry enumeration recognizes daemon records and refuses
active/abandoned registrations, while ordinary registered reads can coexist. A tracking failure
cannot erase this already-durable evidence. The server conservatively leaves ACTIVE on shutdown
until full runtime drain and live-health acknowledgement are integrated; consequently maintenance
is still disabled rather than incorrectly trusting a clean shutdown. Startup registration failure
still needs an externally established fallback fence. Tests verify clean explicit completion admits
maintenance and failed completion survives registry reopening as a blocker.

## Exact live-daemon acknowledgement

Registry admission now accepts an exact healthy live registration by device/inode identity, retaining
a mutable borrow of that registration for the entire maintenance guard. The registration cannot be
failed, finished or dropped while that guard exists. Missing/substituted registration, another
registry's registration, foreign active/abandoned daemons, dirty readers and incomplete scans still
refuse admission. Tests cover local acknowledgement, foreign refusal, post-clean-shutdown admission,
failed health, and cross-root rejection. This capability is not yet wired into async worker dispatch;
its registration-health synchronization and unregistered startup failure path remain unresolved.

## Synchronized tracking failure and live acknowledgement

Server tracking failures now invalidate both the dispatch failure latch and any installed daemon
registration's health. This includes admission failure, timestamp persistence failure, and participant
retirement failure. The registration's ACTIVE evidence is already durable, so invalidating live
acknowledgement requires no successful follow-up disk write. Tests verify a real corrupted tracking
record still permits canonical reads but marks its daemon unhealthy, refuses later live admission,
and remains a maintenance blocker after registry reopening. Startup registration failure and
older-client participation are still unresolved, so production dispatch remains disabled.

## Owned live acknowledgement

Live daemon acknowledgement can now be transferred into owned asynchronous maintenance work. The
token retains the liveness file lock and shares an atomic failure latch with its registration; a
later failure invalidates every token. Clean shutdown refuses outstanding tokens rather than
releasing liveness prematurely. Registry `admit_owned` validates exact file identity and returns an
owned registry gate plus acknowledgement, whose health can be rechecked before publication. A test
verifies failure propagation and lock retention after the original registration handle is dropped.
The storage operations and worker still need to consume this owned admission; dispatch remains gated.

## Owned acknowledgement in conversion operations

Artifact and history cancellation contexts now optionally retain an owned live-daemon token. Their
age-based operations acquire owned registry admission with that token instead of rejecting their
own active registration. Every cancellation checkpoint also rechecks token health, including the
history pre-commit and artifact pre-publication checkpoints. Scheduler passes obtain the token from
the installed registration and reject the local failure latch. A real-artifact test confirms foreign
live daemons prevent compression, foreign clean completion permits local acknowledged compression,
and subsequent tracking failure refuses another operation context. Reclamation still uses offline
admission; the global readiness gate remains false pending startup-failure/older-reader coordination.

## Live-admitted reclamation

Completed scheduler passes now supply the local live acknowledgement to age-fenced database
reclamation, matching artifact and history conversion. Admission and liveness remain held through
engine completion; cancellation/failed health is checked before opening storage and before VACUUM.
The backend VACUUM itself is drained rather than abandoned. The reclamation integration test now
installs a local registration, verifies a foreign active daemon prevents any file shrinkage, then
verifies successful compaction after that foreign registration finishes cleanly. This closes the
self-registration blocker for reclamation; startup dispatch and older/unregistered reader proof
remain unresolved.

## Registration abandonment invalidates owned tokens

Dropping the daemon registration now invalidates all outstanding live acknowledgements. Their
shared file handles retain the OS lock only to drain work; they no longer remain healthy proof after
the registration owner disappears. Attempting clean completion with outstanding tokens also leaves
ACTIVE evidence and invalidates those tokens. Tests verify token health rejection, lock retention
until the last token drops, and preserved dirty evidence. Normal clean completion without tokens
continues to work. Automatic dispatch is still disabled pending startup-failure/older-reader proof.

## Epoch-9 migration through reclamation integration

A database integration test now starts with an epoch-9 fixture, runs the existing exclusively owned
migration coordinator to epoch 10, verifies exact logical JSON/history preservation, compresses the
migrated history, reclaims more than one MiB, reopens the database, and appends another canonical
event. This exercises migration, compression, reclamation and continued writes together rather than
only testing each helper separately. It does not authorize unregistered historical readers or remove
the disabled automatic-dispatch gate.

## Finite artifact sweep boundary

The session domain now exposes candidate pages with an explicit canonical high-water mark. Later
finalizations are excluded from that sweep even if the projection advances; the next sweep can
include them. A regression verifies that continuing a completed old sweep does not chase a newly
finalized artifact. The default caller still captures a new tail each call; scheduler cursor wiring
and the equivalent history high-water bound remain incomplete, so this does not yet resolve queue
fairness for continuously growing sessions or enable automatic dispatch.

## Artifact cursor high-water integration

The worker now retains the captured canonical tail alongside the artifact key cursor and passes it
to every continuation page. A real-session scheduler test appends a new finalized artifact between
pages, verifies the ongoing sweep finishes without touching it, and verifies existing artifacts still
compress and read correctly. This integrates the earlier domain high-water API into actual artifact
pagination. History still needs the same finite sweep boundary; dispatch and startup-failure/older
reader coordination remain incomplete.

## Finite history sweep integration

History maintenance pages now capture and report a canonical tail; the worker retains it across
history continuation ticks and stops at that tail even if later events are appended. Newer events
remain for a subsequent sweep. A real-session regression appends between pages, proves the captured
sweep terminates, and confirms a later sweep still finds the new payload uncompressed. This removes
unbounded tail chasing in history sweeps but does not change the disabled compatibility gate.

## Bounded completed-daemon retirement

Startup now attempts bounded cleanup of cleanly completed daemon records under exclusive registry
admission before registering the new daemon. The complete scan must fit its budget before deletion;
active, abandoned, malformed and unknown records remain untouched. A regression verifies budget
exhaustion makes no cleanup progress, three clean records retire, repeat cleanup is idempotent, and
active/abandoned/unknown evidence survives. This prevents accumulation of explicitly completed
registrations, but production shutdown still needs a proved drain before marking its record clean;
startup-failure and older-reader coordination remain blockers to automatic dispatch.

## History sweep damage handling

History compression now verifies the captured tail still exists and each returned row matches the
expected contiguous sequence. A missing or regressed tail, an empty page before the captured end,
or a gap rejects the transaction instead of resembling normal completion. A regression removes a
middle row, verifies earlier compression rolls back, and checks a missing captured tail is rejected.
This preserves damage visibility during finite sweeps; it does not alter the disabled dispatch gate.

## Strict daemon inventory for coordination

Daemon lifecycle now exposes a bounded strict inventory distinct from best-effort cleanup discovery.
Missing/unreadable registries, malformed/future records, unexpected entries, oversized records,
filename identity mismatches, duplicate namespaces/instances and exhausted scan budgets return errors
rather than an empty/partial list. Tests confirm best-effort cleanup remains separate. This inventory
is only an observation: registration locking, endpoint/process identity verification and descriptor
confinement against concurrent path mutation are still required before it can authorize absence of
older readers. The automatic dispatch gate is unchanged.

## Complete live acknowledgement sets

Owned registry admission now accepts an explicit set of live daemon tokens, requiring each token to
match exactly one record and every other live daemon to be absent or cleanly completed. Missing,
duplicate, cross-root and failed tokens reject admission. The returned guard retains all liveness
handles and its health check fails if any participant fails. A regression covers two live daemons,
incomplete/duplicate sets, successful complete admission, and later participant failure. Collecting
such proofs across independent daemon processes is still unimplemented; this API alone does not
resolve older-reader coordination or open the dispatch gate.

## Durable live-acknowledgement validation

Exact live acknowledgement now validates the durable registration length and ACTIVE bytes using
positional IO, in addition to device/inode identity and shared health. A live in-memory token cannot
silently authorize a truncated, future-version, or unexpectedly completed registration file. Tests
mutate those bytes while the registration remains live and verify admission refuses without repair.
This strengthens the existing admission mechanism; dispatch remains gated pending startup-failure
and older-reader coordination.

## Clean completion preserves damaged registration

Daemon registration `finish` now verifies the exact durable ACTIVE representation before writing
CLEAN. Truncated, future-version, or unexpectedly completed bytes are preserved and rejected instead
of being silently repaired by shutdown. Outstanding tokens and tracking failures continue to refuse
completion. A regression covers each altered representation and proves lock release without data
replacement. Production clean-shutdown drain and cross-process coordination remain incomplete; the
automatic dispatch gate is unchanged.

## Final-state shutdown registration completion

Registration is now retained until final ServerState destruction. A requested healthy shutdown
attempts CLEAN only after the last state owner releases; outstanding acknowledgement tokens cause
completion to refuse and preserve ACTIVE. Abnormal drop and failed tracking also preserve ACTIVE.
Tests retain another state owner across shutdown and prove maintenance stays blocked until final
release, then verify clean retirement; another test retains a token and proves dirty evidence stays.
This closes normal registration cleanup without claiming that older/unregistered daemons participate.
The global automatic-dispatch gate remains disabled.

## Startup registration fallback

Startup registration failure now uses the same durable maintenance blocker as failed read
admission, before the client accept loop starts. Both paths latch local tracking failure even
when blocker persistence fails. Tests cover blocker survival after healthy shutdown/restart and
an unavailable registry without modifying the obstructing file. Registration now runs before
background services, workflow recovery, and ready callbacks in `run_constructed_server`, rather
than immediately before the accept loop. A startup lifecycle test verifies that foreign maintenance
is blocked until clean shutdown; the unavailable-registry test exercises registration failure itself.
This does not establish an older-reader fence or make double-failure fallback safe for automatic
dispatch; the compatibility gate remains closed.

Validation: `cargo fmt`, `cargo check --workspace --quiet`,
`cargo clippy --workspace --all-targets --quiet -- -D warnings`, and
`cargo test -p bcode_server storage_read_admission::tests --quiet` passed (7 tests).

## Daemon maintenance authority

Daemon maintenance now requires a live registration acknowledgement; missing registration no
longer falls back to offline maintenance admission, even when the registry is otherwise clean.
Scheduler fixtures use the production startup registration boundary. A regression removes a
clean registration and verifies that an eligible artifact remains raw and scheduling fails closed.
Explicit offline session maintenance APIs are unchanged. This closes a local authority fallback,
not the independent older-reader or durable fallback-failure compatibility gaps.

Validation: `cargo fmt`, `cargo check --workspace --quiet`,
`cargo clippy --workspace --all-targets --quiet -- -D warnings`, and
`cargo test -p bcode_server storage_maintenance::tests --quiet` passed (11 tests).

## Read admission before existence lookup

Persistent read admission no longer skips registration based on `session.db` existence. An absent
file or failed metadata lookup cannot prove that a later lookup will not consume content. A
regression verifies that a missing-session read blocks maintenance until completion without
creating canonical session storage. This closes an admission bypass, not the older-reader
compatibility gap; automatic dispatch remains disabled.

Validation: `cargo fmt`, `cargo check --workspace --quiet`,
`cargo clippy --workspace --all-targets --quiet -- -D warnings`, and
`cargo test -p bcode_server storage_read_admission::tests --quiet` passed (8 tests).

## Raw-reader compatibility evidence

The artifact publication regression now retains a raw file descriptor across light and deep
conversion, including backup cleanup. Reads at the start, a chunk boundary, and the tail still
return original bytes through that descriptor; reopening the logical path as a raw file fails
because the authoritative representation is a directory. Updated range reads return identical
logical bytes. This tests actual filesystem behavior, not just version markers. It does not prove
access-age tracking for old readers or canonical-history compatibility, and is not a complete
historical-binary integration test. Automatic dispatch remains disabled.

Validation: `cargo fmt`, `cargo check --workspace --quiet`,
`cargo clippy --workspace --all-targets --quiet -- -D warnings`, and
`cargo test -p bcode_session artifact_storage::tests::publishes_at_same_path_and_reads_original_bytes --quiet`
passed (1 test).

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
Current event payloads support a versioned compressed envelope as well as raw JSON. Bounded
history recompression and explicit database reclamation are implemented. The
`history_compression::tests` suite verifies legacy JSON decoding rejects a compressed payload,
current reads preserve the logical event, a multi-page sweep reclaims database space, reopening
and appending preserve history, captured tails exclude later appends, and incompatible writers
cannot recompress canonical payloads. All four tests passed in the latest focused validation.
These tests do not establish exclusion of historical readers: rejecting a compressed event alone
does not prevent an old client from consuming uncompressed events or derived projections.
Migration-owned minimum-reader enforcement and the audit of original usage, export, repair, and
bounded search projection ingestion remain prerequisites to automatic rollout.

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
