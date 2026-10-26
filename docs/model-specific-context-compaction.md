# Model-specific context compaction

Bcode applies automatic compaction policy to an effective concrete model ID. Unspecified
model-specific fields inherit from `[model.compaction]`.

```toml
[model.compaction]
mode = "auto"
backend = "auto"
proactive_threshold_percent = 90
keep_recent_tokens = 20000
experimental_adaptive_packing = false

[model.compaction.models."gpt-5.6-sol"]
provider_plugin_id = "bcode.amazon-bedrock"
mode = "proactive_and_overflow"
proactive_threshold_tokens = 268000
```

Use the reported provider and effective model IDs. Optional `provider_plugin_id` prevents an
override matching an identically named model from another provider. Absolute
`proactive_threshold_tokens` takes precedence over percentages and measures the estimated complete
candidate input request. The trigger is capped at safe input capacity after output reserve and
safety margin. Billing accounting can differ: leave a buffer below hard pricing boundaries.
Defining a threshold alone does not enable proactive compaction; select `proactive` or
`proactive_and_overflow`. Backend `auto` prefers supported provider-native compaction, otherwise local.

## Continuation safety

Recent retention preserves complete tool exchanges, not necessarily an entire long-running user
turn. Parallel batches and unresolved calls stay together. Local synthesis processes chronological,
attributed source fragments from complete selected conversational units. Oversized user directives
are fragmented without dropping their endings. Tool results use bounded, untrusted context
projections; compaction does not preserve every artifact byte.

Structured summaries separate current user scope with canonical source references from derived
continuation facts. Only user corrections can supersede conflicting user scope; assistant or tool
claims cannot authorize work or remove restrictions. Summaries are assistant recollection, never
system instructions. Legacy attribution is not invented. Existing scope/supersession validators
remain in the synthesis path; incomplete, invalid or oversized output is rejected, not clipped.
Request-only guidance and assembled provider requests are not portable summary sources.

## Implemented durable local checkpoint path

The current implementation is in `packages/server/src/context_compaction_checkpoint.rs`, with
session contracts in `packages/session/models/src/compaction_checkpoint.rs` and
`packages/session/src/compaction_checkpoint.rs`. These are operational evidence, not a second
history store or execution authority. Original canonical history remains authoritative; only a
committed compaction marker changes model context.

The actual `SessionManager` APIs are:

* `session_compaction_checkpoint(session_id, key)`: bounded exact-key canonical lookup.
* `append_compaction_checkpoint(session_id, key, payload)`: serialized operational append;
  `None` records retirement without erasing evidence.
* `commit_compaction_checkpoint(session_id, CompactionCheckpointCommit)`: validates expected
  checkpoint sequence, prior marker and ordered source identities, then atomically publishes the
  final marker and checkpoint retirement. Final publication requires persisted storage.

The version-1 `CompactionSynthesisCheckpoint` envelope is a reserved canonical
`TraceEvent::ProviderEvent`, independent of optional tracing. It has a bounded operation key
(1,024 UTF-8 bytes) and host-versioned payload (256 KiB maximum). Session storage enforces envelope
bounds; the host interprets source, carry and allowances. A disposable indexed projection locates
the latest exact-key event and verifies it against canonical evidence. Missing, corrupt or
incompatible required state fails closed, without recent-history scanning or read-time reindexing.
Storage uses the existing typed Switchy/session transaction path, not new raw SQL. Writer/projection
compatibility and migration are session-owned; see [session persistence](session-persistence-architecture.md).

The operation namespace includes the prior marker, concrete provider/model, authentication-profile
reference, tool projection limit, planning policy, and synthesis prompt/format. It excludes the
active turn and newly selected prefix: appended follow-ups do not hide accepted work or reset its
budget. The host freezes the covered boundary, canonical source digest and chunk-digest manifest;
resume reconstructs that boundary and verifies source, manifest and packing policy. This is not a
promise to accept arbitrary configuration changes or a general source-generation certificate.
Publication accepts at most 4,096 source identities and checks them transactionally. New tail events
remain outside the frozen boundary; a changed marker/source conflicts rather than silently rebasing.

`LocalCheckpoint` retains structured evidence, next chunk, user-source and unfinished-source sets,
and per-chunk request counts. Accepted carry is saved only after the existing fragment validator
passes and unfinished-source bookkeeping is updated. Later fragments may reconcile the same
unfinished directive; finalized or unrelated restrictions remain protected. Resume restores these
sets rather than inferring completion from summary text. No streaming delta becomes accepted carry.

Each provider attempt reserves and saves a slot before dispatch, including repairs and ambiguous
interrupted attempts. Accepted chunks are skipped on resume; reservations are not refunded.
The two-attempt-per-chunk limit survives cancellation/restart. The existing bounds remain: 32 base
chunks, 2,000-byte fragments, legacy 16,000 input bound, 6,000 carried-summary characters and UTF-8
byte guard. These are not interchangeable token units. The implementation does **not** provide the
previously proposed six independent reservation/creation/acceptance APIs, nor a general durable
ledger of elapsed time, decoded bytes or every recovery allowance.

Cancellation is checked through provider processing and final publication under the marker-commit
gate. Invalid synthesis, unfinished sources, cancellation or insufficient net reclaim leave the
old marker unchanged; previously durable carry can be reused by later admitted work. Atomic marker
plus retirement prevents a half-published boundary. Normal ownership, admission and policy checks
still govern execution: a checkpoint cannot reopen a cancelled turn, prove an ambiguous call had
no effects, or authorize automatic daemon recovery.

## Experimental adaptive packing

`experimental_adaptive_packing` is implemented and ships **disabled by default**, including inherited
model-specific opt-in support. Disabled mode preserves legacy chunks. Enabled mode merges complete
chronological chunks only when complete original and repair requests fit the selected model's safe
input capacity, with a conservative carried-summary reserve. Actual synthesis requests are checked
again against capacity. Fragments are not clipped, reordered or split by packing. Packing identity
is checkpointed; switching policy cannot reinterpret retained work. This changes request packing,
not recent-tail retention, and is experimental pending semantic quality evaluation.

Native compaction is not made resumable by local checkpoints. Explicit native recompaction includes
the previous marker, retaining existing provider/model/authentication/format compatibility and
portable fallback behavior. Local carry cannot certify an interrupted native request or opaque
snapshot as reusable. Request-only data must not leak through either path.

## Evidence and remaining gaps

The offline gate is `scripts/check-compaction-eval.sh`: verifier tests, repeated explicit compaction,
and automatic cancellation/restart/crash cases using an identified fake-provider binary and isolated
state. The coordinator reports all cases passing on artifact prefix `af4ee8de`, plus 34 engine tests
and 101 migration tests; this documentation lane did not rerun those commands. Restart/crash cases
pass **after explicit targeted cancellation of the interrupted old turn**, followed by new admission.
They prove checkpoint reuse through those lifecycle paths, not complete automatic daemon resume.

Fake-provider fixtures establish structural coverage, attribution, cancellation, durable carry,
request-only exclusion and actual post-marker request composition. Their canned marker language
is not evidence of natural-language understanding. No live semantic retention evaluation has run.
The [retention corpus and scoring protocol](../fixtures/evals/compaction/README.md#natural-language-retention-evaluation)
predeclare that separate comparison; provider spending requires explicit approval. Adaptive quality,
automatic daemon recovery without targeted cancellation, and semantic retention across real-model
repeated compactions remain unproven. Structural validators do not make compaction lossless.

## Reconstructing an already stale summary

Ordinary compaction cannot recover evidence omitted by an earlier summary. Explicitly use
`session compact ID --rebuild-summary --expected-marker SEQ` to reconstruct the selected latest
marker from canonical source history. The owning session's serialized command path replaces the
summary at the same boundary, leaving the uncompacted tail intact. Pending submissions and old
summaries are not source evidence.

Recovery has explicit `--rebuild-max-events`, `--rebuild-max-pages`, `--rebuild-max-decoded-bytes`,
`--rebuild-max-provider-requests`, and `--rebuild-timeout-ms` bounds. Invalid/exhausted allowances,
changed source generations, cancellation, missing evidence or invalid synthesis leave the marker
unchanged. It may call the selected model; it is neither normal-read repair nor guaranteed semantic
recovery. Conflicts require a new explicit selection, not silent retry.
