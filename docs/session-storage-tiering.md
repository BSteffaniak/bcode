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

## Remaining implementation

### Access policy and scheduling

* Persist application-owned, coalesced usage metadata separately from canonical history. Ordinary
  reads must not rewrite canonical events, perform repair, or trigger format migration.
* History hydration and artifact range reads count as access; catalog listings, compression scans,
  and background indexing do not. Define treatment of model-context reads explicitly.
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
