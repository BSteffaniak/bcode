# Deterministic compaction controls

These fake-provider settings are opt-in scratch test controls, not host APIs or a model of
natural-language retention. Normal requests and cache simulation ignore them. Existing
`fake_compaction_fixture = "scope-v1"` source attribution and marker semantics are unchanged.

```toml
fake_compaction_control_dir = "/absolute/existing/test-owned/scratch"
fake_compaction_pause = "true"
fake_compaction_pause_timeout_ms = "30000"
fake_compaction_outcome = "success"
```

The harness must select an existing absolute **canonical** scratch directory, never a session
storage root. Resolve platform aliases such as macOS `/var` before configuring it; symlink aliases
and `..` paths are rejected. Attempts are rechecked for canonical confinement before file writes
and each gate poll; replacing an attempt directory with a symlink fails closed.
Each compaction request creates a private unique `attempt-*` child. Discovery order is not
chronology: correlate the captured request and source sequence/offset, not directory names.
Attempts are never reused across process restarts. The harness owns cleanup.

Wait for `ready.json` before inspecting an attempt. Its presence means capture is complete,
not that synthesis was accepted or a host checkpoint/marker was committed. Files are published
by temporary-file rename:

* `request.json`: exact serialized normalized request fields except `provider_context`, which
  is deliberately excluded to avoid capturing credential custody/settings. Includes exact
  message/system/tool content and metadata; opt-in capture can contain private source content.
  This is model-input evidence, **not** a full provider wire request or an authentication record.
* `sources.jsonl`: exact attributed JSONL source suffix from the synthesis prompt, excluding
  only the known host repair-instruction suffix. Fragment text is never clipped or reattributed.
* `coverage.json`: per-record sequence, provenance, byte offset, final-fragment flag, UTF-8 text
  byte length and SHA-256. This describes delivered fragments, not semantic retention.
* `ready.json`: `format = "bcode.fake.compaction-control.v1"`, SHA-256/byte lengths of
  request/source files, excluded request fields, pause/outcome/correction flags.

When paused, create an empty regular `release` file **inside the selected attempt**. A symlink
or directory does not release the gate. Cancellation is polled every 10 ms. The timeout is
1–300000 ms, default 30000 ms. Each bounded repair gets its own attempt and release gate.
No automatic release is inferred from an older attempt or a preexisting root-level file.

`fake_compaction_outcome` selects:

* `success`: unchanged synthesis/fixture result.
* `fail`: deterministic nonretryable structured-output provider error.
* `malformed`: invalid JSON on every attempt.
* `repair`: malformed initial response; unchanged synthesis on a correction request. Recognizes
  existing structured-result correction metadata and the exact bounded compaction repair suffix.
* `oversize`: structurally valid synthesis with a deliberately oversized continuation (>64 KiB).
  Existing scope references remain untouched; host validation must reject rather than clip it.

`finished.json` reports `response_ready` (`response_ok`), `cancelled`, or
`timeout_or_io_error`. This is provider-side evidence only, never a host success/commit receipt.
Process death may leave ready evidence without a finished record. A failed scratch write fails
closed with a normalized error (paths and request content are not embedded in public errors).

Tests (run by the coordinator):

```text
cargo test -p bcode_fake_provider_plugin compaction_control
cargo test -p bcode_fake_provider_plugin compaction_fixture
cargo test -p bcode_fake_provider_plugin cache_model_compaction_uses_opt_in_attributed_fixture_not_cache_simulation
```

The controls do not establish durable compaction resume, host publication safety, or instruction
retention by themselves; the harness must separately inspect authoritative host outcomes.
