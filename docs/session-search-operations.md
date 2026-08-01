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

Sensitive content must be both provider-allowlisted and explicitly targeted. Large shell/tool output
search is not yet an implemented provider workflow; do not infer deep corpus coverage merely because
`--deep` is accepted by the backend-neutral planner.

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

Historical backfill is explicit and bounded. Select exact sessions when possible:

```sh
bcode session search-backfill --provider bcode.tantivy-session-search \
  --session SESSION_ID --deadline-ms 30000 --json
```

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
* corrupt/degraded provider state: preserve canonical storage, purge or rebuild only the provider
  root, then backfill.
* canonical `repair_required` or incompatible hydration: use `session diagnose`, `session doctor`,
  and explicit canonical repair/migration commands. Never use a derived index to repair canonical
  history.
* deletion races: canonical deletion wins; provider generation tombstones reject stale ingestion
  that could otherwise recreate deleted derived records.

Provider search, ingestion, corruption, quota, and maintenance failures must never delay, roll back,
or replace canonical session writes.

## Measured limits

The deterministic Tantivy provider baseline now covers both 25,000 and 100,000 records. The final-size
100,000-record run measured 1.063 ms p50 and 1.260 ms p95 warm query latency, 78.653 ms p50 and
132.091 ms p95 commit latency per 250-record batch, 3.943 ms reopen time, and 17.3% index
amplification using a 15 MiB writer cache. These results satisfy the initial 100 ms ordinary-query and
50% amplification budgets. Commit latency belongs to detached asynchronous provider work, not the
canonical append path. Peak RSS, canonical hydration, daemon startup modes, and daemon-level
p50/p95/p99 measurements remain outstanding; see `docs/session-search-baseline.md`.
