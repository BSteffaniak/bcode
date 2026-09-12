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

## Configured policy (not activated)

The configuration loader accepts and validates:

```toml
[session_storage]
enabled = true
light_after_days = 5
deep_after_days = 30
```

These are the defaults. Thresholds require `0 < light_after_days < deep_after_days`, including when
disabled; unknown fields and invalid values are rejected. Days mean elapsed 24-hour durations.
The settings are documented by the configuration schema but **do not yet start maintenance**.

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
