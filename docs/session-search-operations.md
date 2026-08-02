# Session Search Operations

Bcode session search is an optional derived capability. Canonical session history remains owned by
the daemon and remains available through `session list`, `session history`, `session around`,
`session inspect`, and explicit `session export` when every search provider is absent or disabled.
Never open `catalog.db`, `session.db`, WAL files, provider checkpoint files, or provider indexes to
investigate a session.

## Enablement and storage

Global search execution and ingestion are controlled independently of plugin selection:

```toml
[session_search]
enabled = true
```

Set `enabled = false` to suppress provider inventory, queries, maintenance, dirty-session scheduling,
and ingestion-worker startup while preserving canonical investigation commands. Disabling search or
a provider does not delete retained derived data.

The optional bundled Tantivy provider must also be selected and configured. Plugin settings are
owned by that provider:

```toml
[plugins]
enabled = ["bcode.tantivy-session-search"]

[plugins.config."bcode.tantivy-session-search"]
storage_root = "/absolute/path/to/bcode-derived/session-search/tantivy"
quota_bytes = 2147483648
writer_memory_bytes = 33554432
sensitive_content = ["assistant_reasoning"]
```

`storage_root` must be an absolute, confined, non-root directory. Do not place it inside a canonical
session directory. The provider stores only disposable derived state under that root. Its default
quota is 2 GiB and its default writer cache is 32 MiB; writer memory below 15 MiB is rejected.
Changing quota or content policy makes retained state incompatible until explicit rebuild.

The default content policy includes session titles, user/assistant/system messages, shell command
text, tool errors, and compaction summaries. Reasoning, shell output, tool arguments, successful tool
output, permissions, runtime diagnostics, trace metadata, and artifact metadata require explicit
provider allowlisting. Enabling a sensitive category copies bounded normalized text into provider
storage until purge.

## Investigation and query workflows

List canonical sessions and inspect bounded canonical events:

```sh
bcode session list --json
bcode session history SESSION_ID --limit 100 --json
bcode session around SESSION_ID SEQUENCE --before 20 --after 20 --json
bcode session inspect SESSION_ID failed-tool-calls --limit 50 --json
```

Search transcript terms or an exact phrase:

```sh
bcode session search 'database locked' --json --hydrate
bcode session search 'permission denied' --match phrase --content tool-error --json
```

Search failed commands and restrict by working directory or canonical timestamp:

```sh
bcode session search 'exit status' --content shell-command --content tool-error \
  --tool-status failed --json
bcode session search 'migration' --working-directory /workspace/project \
  --after-timestamp-ms 1767225600000 --before-timestamp-ms 1769817600000 --json
```

Sensitive content must be both provider-allowlisted and explicitly targeted. The shared planner
rejects `shell_output` and `tool_output` in ordinary mode, so those categories always require
`--deep`. Large shell/tool output search is not yet an implemented provider workflow; do not infer
deep corpus coverage merely because `--deep` is accepted by the backend-neutral planner.

```sh
bcode session search 'credential refresh' --content assistant-reasoning --json
bcode session search 'segmentation fault' --content shell-output --deep --json
```

Use explain-plan before expensive or sensitive queries:

```sh
bcode session search-explain 'segmentation fault' --content shell-output --deep --json
```

Search JSON distinguishes query completion from corpus coverage. Always inspect `outcome`,
`query_complete`, `coverage_complete`, provider reports, failures, exclusions, and hydration outcome.
No results with incomplete coverage is not proof that the text is absent.

## Status and maintenance

Inspect capabilities, versions, quota, document count, degraded state, and per-session coverage:

```sh
bcode session search-status --json
```

Historical backfill is explicit, bounded, and optionally addressable. For a short synchronous request,
select exact sessions when possible:

```sh
bcode session search-backfill --provider bcode.tantivy-session-search \
  --session SESSION_ID --deadline-ms 30000 --json
```

For user-cancellable work, start an addressable operation and use its returned operation ID:

```sh
bcode session search-backfill-start --provider bcode.tantivy-session-search \
  --session SESSION_ID --deadline-ms 30000 --json
bcode session search-backfill-status OPERATION_ID --json
bcode session search-backfill-wait OPERATION_ID \
  --after-revision REVISION --timeout-ms 30000 --json
bcode session search-backfill-cancel OPERATION_ID --json
```

Operation revisions and wait notifications are bounded in-process state. They are intentionally lost
when the daemon restarts and do not define reconnect-safe or durable resume. Provider-owned sequence
and text checkpoints are the durable continuation boundary; after restart, issue a new bounded
backfill operation, which resumes from those checkpoints.

A catalog-wide request selects at most 256 sessions per call. Apply timestamp bounds and continue
with the returned `next_cursor` (`UPDATED_AT_MS:SESSION_ID`) until `selection_truncated` is false:

```sh
bcode session search-backfill --provider bcode.tantivy-session-search \
  --after-timestamp-ms 1767225600000 --deadline-ms 30000 --json
bcode session search-backfill --provider bcode.tantivy-session-search \
  --cursor UPDATED_AT_MS:SESSION_ID --deadline-ms 30000 --json
```

Backfill resumes from provider-owned sequence/text checkpoints and reports each selected session's
indexed-through sequence and canonical tail. Reissuing bounded work is checkpoint-safe; it is not a
claim of reconnect-safe or transport-level durable resume. Deadline, incomplete, or failed outcomes
must be inspected before claiming complete coverage.

Rebuild discards provider state and creates an empty compatible index. It does not backfill history:

```sh
bcode session search-rebuild --provider bcode.tantivy-session-search \
  --confirm rebuild-bcode.tantivy-session-search --json
```

Run explicit backfill after a successful rebuild. Purge removes only provider-owned derived data and
does not mutate canonical sessions:

```sh
bcode session search-purge --provider bcode.tantivy-session-search \
  --confirm purge-bcode.tantivy-session-search --json
```

## Recovery guidance

* `disabled` or no provider: use canonical investigation, then enable global search and the selected
  provider if derived search is wanted.
* `stale` or incomplete coverage: run selected/time-bounded backfill and inspect terminal progress.
* quota exhaustion: increase the configured quota or purge/rebuild intentionally; no automatic
  eviction occurs and completeness remains false.
* incompatible policy, normalization, record, tokenizer, or index version: rebuild, then backfill.
* interrupted rebuild: provider status reports `rebuilding` from a confined provider-owned marker and
  normal provider use fails closed. Retry the exact confirmed rebuild; after it creates a compatible
  empty index and clears the marker, run bounded backfill. The marker is lifecycle evidence, not a
  replayable operation log.
* corrupt/degraded provider state: preserve canonical storage, purge or rebuild only the provider
  root, then backfill.
* canonical `repair_required` or incompatible hydration: use `session diagnose`, `session doctor`,
  and explicit canonical repair/migration commands. Never use a derived index to repair canonical
  history. Search ingestion refuses active canonical migration. After repair or truncation, a
  provider checkpoint ahead of the canonical tail or bound to an older writer/event/projection
  generation requires explicit provider rebuild followed by bounded backfill.
* Imported canonical Bcode sessions may be filtered by stable import source ID. Native search records
  do not contain source database paths or external source-session persistence details. Use the
  matching source-specific history skill when the original source-native conversation is required.
* deletion races: canonical deletion wins; provider generation tombstones reject stale ingestion
  that could otherwise recreate deleted derived records.

Provider search, ingestion, corruption, quota, and maintenance failures must never delay, roll back,
or replace canonical session writes.

## Selected large-output provider limits

The selected deep-search provider is an independently compressed in-process chunk scanner. Before it
may be enabled, implementation and tests must enforce:

* 256 KiB maximum uncompressed normalized UTF-8 per chunk and 64 records per chunk;
* 128 records per invocation, 256 MiB normalized text per session, and 8 GiB compressed provider
  quota;
* SHA-256 checksums for compressed and normalized bytes plus versioned atomic manifests;
* 64 MiB decompressed cache, two concurrent scans, 200 hits, 4 KiB previews, and request deadlines;
* no automatic eviction—quota refusal preserves retained data and reports incomplete coverage;
* literal/phrase and explicitly advertised bounded Rust-regex semantics, not claimed `rg`
  compatibility;
* cancellation checks around chunk open, decompression, scanning, and hit emission;
* isolated degraded coverage for missing/corrupt/partial chunks rather than silent empty results.

These are provider-owned derived-state limits. They do not expand canonical read bounds or permit the
provider to open canonical storage.

## Measured limits

The deterministic Tantivy provider baseline now covers both 25,000 and 100,000 records. The final-size
100,000-record run measured 1.063 ms p50 and 1.260 ms p95 warm query latency, 78.653 ms p50 and
132.091 ms p95 commit latency per 250-record batch, 3.943 ms reopen time, and 17.3% index
amplification using a 15 MiB writer cache. A fresh already-built release test process measured
70,074,368 bytes (66.8 MiB) maximum RSS and 33,980,944 bytes peak memory footprint under macOS
`/usr/bin/time -l`; this is a conservative whole-provider-process value including the test harness,
record construction, ingestion, query, and reopen phases, not coordinator-only incremental RSS.
These provider results satisfy the initial 100 ms ordinary-query and 50% amplification budgets. A
grouped production-path hydration benchmark measured 0.510/1.129 ms p50/p95 for 20 exact locators
over a 1,000-event persistent session, within the 100 ms bounded-read budget. Nearby same-session
locators use bounded inclusive reads while sparse gaps split; missing exact sequences remain stale.
A 10,000-event ordinary-append fixture exceeded a 20-minute setup envelope before measurement, so
larger representative application/daemon distributions remain open. Commit latency belongs to
detached asynchronous provider work, not the canonical append path. Daemon startup modes and
daemon-level p50/p95/p99 measurements remain outstanding; see `docs/session-search-baseline.md`.
