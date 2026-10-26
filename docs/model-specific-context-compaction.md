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

### Host-preserving synthesis updates

Local synthesis uses `packages/server/src/context_compaction_synthesis.rs`. The model is asked for
`bcode.compaction.scope-update.v1`, containing `current_scope`, `reconciles`, and `continuation`.
This is a response contract, not a new durable summary format: the host merges accepted updates
into the existing `bcode.compaction.scope.v2` envelope.

* Omitted unchanged scope is copied verbatim by the host. Exact echoes are allowed; paraphrasing
  established scope under its completed source is rejected rather than silently normalized.
* New scope cites eligible canonical user sequences. A newer user source contributing this chunk
  may declare older sources in `supersedes`. These are per-response changes, not an accumulating
  authorization history. Updates reject sequence-only supersession when that source contains
  multiple distinct statements: it cannot safely identify which independent restriction changed.
  Repair retains the protected scope and records the correction as unresolved rather than
  inventing broader permission. Legacy full envelopes retain their existing group-replacement
  compatibility semantics.
* `reconciles` explicitly names unfinished user sources contributing further fragments in this
  chunk. Each named source needs replacement scope under the same canonical sequence, including
  when its final fragment corrects a provisional interpretation. Completed or unrelated sources
  cannot be reconciled. Unfinished status advances only after acceptance.
* `continuation` replaces the previous derived continuation. The prompt asks the model to carry
  forward relevant evidence, blockers and unresolved work; the host does not independently prove
  that this replacement retains every fact.

Compatibility with full `bcode.compaction.scope.v2` responses remains strict: omission of unchanged
established scope is an error, not an implicit update. A full response may replace scope only through
eligible supersession or unfinished-source reconciliation; reconciliation cannot silently retire
an established provisional source without replacement scope. Free-text legacy summaries remain
unverified derived input, not newly attributed user authorization.

Supersession currently replaces an entire source-sequence scope group. The prompt requires the
replacement to preserve independent applicable restrictions, but the validator does not mechanically
prove that a partial correction retained every other restriction from that same source. Host copying
protects unchanged groups; it does not establish semantic completeness of new scope, replacements or
continuation. Keep this limitation distinct from successful structural validation.

### Carry budgets and bounded repair

The prompt reports the remaining JSON-encoded continuation allowance after serializing established
scope with an empty continuation. Escapes count toward that allowance; it is neither a token budget
nor a promise that new scope will fit. Acceptance checks the complete merged envelope against the
6,000-character carry limit, with a separate UTF-8 wire byte guard. Adding scope consumes continuation
space. Protected scope is not clipped or silently discarded to fit: if scope plus a nonempty valid
continuation cannot fit, synthesis can fail and leave the marker unchanged. Host preservation reduces
unnecessary regeneration; it does not remove the capacity ceiling or guarantee compaction progress.

Repair uses typed rejection reasons, including invalid JSON, unsupported format, invalid source or
supersession, unauthorized scope changes, missing scope, invalid reconciliation, and wire/carry
budget failures. The repair prompt retains the complete base carry and selected source fragments.
It attaches the rejected response whole as an untrusted diagnostic JSON string only within the
attachment allowance; otherwise it explicitly reports the attachment unavailable or oversized.
Rejected output is not canonical evidence. After a restart without a retained rejected response,
the repair prompt does not assume that the previous reserved attempt succeeded.

Repair still consumes the existing second durable slot for that chunk, not a new retry allowance.
Exhaustion reports a rejected synthesis and leaves the old marker unchanged; accepted earlier chunks
remain durable. Experimental packing budgets the real repair formatter, bounded diagnostic escaping
and reconciliation eligibility as well as the original request, then checks actual request capacity
again. Capacity checks can still reject work; reservations are not refunded.

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
  final marker and checkpoint retirement. Persistent sessions use the storage transaction;
  in-memory sessions validate and publish both events in one serialized actor command, without an
  intervening await. In-memory evidence does not survive process loss.

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
and per-chunk request counts. Accepted carry is saved only after merged synthesis validation passes
and unfinished-source bookkeeping is updated. Later fragments may reconcile the same unfinished
directive; unrelated established scope remains subject to preservation/supersession checks. Resume
restores these sets rather than inferring completion from summary text. No streaming delta becomes
accepted carry.

Decode rejects inconsistent retained acceptance state before automatic recovery consults its cursor:
accepted carry needs nonempty continuation and an exact user-source set equal to retained scope
sources plus unfinished sources; scope statements must be nonempty and cite retained users. Cursor,
request-count, format and carry bounds are also checked. Full source/manifest verification remains a
separate requirement for synthesis resume. A cursor or reserved request alone is not proof that a
chunk was accepted, and decode integrity does not establish semantic retention or execution authority.

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
still govern execution: accepted carry alone cannot reopen a cancelled turn, prove an ambiguous call
had no effects, or authorize automatic daemon recovery. Original-activation recovery uses a separate
protected checkpoint.

## Protected automatic compaction recovery

Automatic proactive and overflow compaction can persist a version-2 provider-retry checkpoint,
implemented in `packages/server/src/compaction_recovery.rs` and
`packages/server/src/provider_retry_recovery.rs`. This is distinct from the version-1 local
synthesis envelope. It binds the original activation, retry counters, round/output positions and
selected compaction plan to a protected phase. Manual compaction does not acquire this automatic
turn-recovery authority.

Recovery verifies the original execution descriptor and session/workflow ownership; it does not
substitute the replacement daemon's configuration. Config-recovery sources remain confined to
verified allowed roots; private restart fixtures keep their config inside the session root rather
than relaxing confinement. A recovery dispatch matches the activated FIFO record by session ID,
activation sequence, submission ID and turn target. It cannot skip the oldest unresolved
submission, activate input twice, or replay tool work. Terminal turn evidence stays terminal.

A `Safe` phase can resume. A `Local` dispatch can resume only if canonical accepted-chunk evidence
proves that exact reserved chunk was durably accepted. A reservation alone is insufficient.
Interrupted native dispatch and local dispatch without accepted evidence terminate with an
unresolved-outcome error and do not replay the request. Cancellation retains cancellation semantics.
Once the old activation settles, later admitted work may reuse valid local carry; this is not proof
that the interrupted provider call had no effects. Retained native snapshots still require normal
provider/model/authentication/format compatibility checks.

Overflow recovery can additionally retain an optional bounded diagnostic from the normalized
provider error: code and message are control-character-filtered and capped at 128 and 1,024
characters respectively. Failure can report the original overflow alongside the recovery failure.
Older records without that diagnostic remain supported. These operational details are not exposed
by the secret-safe compaction inspection page.

## Bounded compaction inspection

`bcode session compaction-status <session-id> [--before SEQ] [--limit N] [--json]` reads typed
historical compaction evidence without restoring or executing a checkpoint. The default limit is
50, with 1..=200 indexed candidates per page, not matching entries. Follow the returned continuation
cursor unchanged, including after an empty page with `has_more`. Byte and structural exhaustion,
unsupported evidence and failed validation are reported as degraded coverage, not guessed progress.

The versioned page exposes closed phase/resume/blocker values and optional validated counters.
Older lifecycle evidence does not imply recorded counters or resumability. It omits operation keys,
summary/carry text, source manifests, provider diagnostics and opaque checkpoint payloads; historical
terminal entries are not live work. This bounded observation is neither execution authority nor
repair and does not replay history.

Canonical history/range/source-page and export APIs retain exact event evidence. Semantic
presentation uses payload-free copies of operational compaction-synthesis checkpoints instead of
publishing private carry; redaction does not rewrite or retire canonical records. Request-only
context remains excluded from durable checkpoint and marker content.

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
state. Restart/crash fixtures retain a genuine original execution descriptor with private config
inside the session root; config confinement must not be relaxed to make a fixture recoverable.
The recorded restart/crash cases exercise original-activation unresolved-outcome settlement without
targeted cancellation, then later admission and accepted-carry reuse. They do not demonstrate replay
of ambiguous work or lossless semantic retention.

Fake-provider fixtures establish structural coverage, attribution, cancellation, durable carry,
request-only exclusion and actual post-marker request composition. Their canned marker language
is not evidence of natural-language understanding. No live semantic retention evaluation has run.
The [retention corpus and scoring protocol](../fixtures/evals/compaction/README.md#natural-language-retention-evaluation)
predeclare that separate comparison. The executable corpus adapter currently uses independent
accumulated-prefix diagnostic trials: the supported CLI cannot append role-faithful assistant/tool
batches to an existing session. Enforceable provider-spending budgets and other frozen-protocol
boundaries remain barriers; operation/wall limits do not substitute for a spending cap. Live mode
remains blocked, not merely awaiting a flag. Adaptive packing remains default-off; real-model
quality and semantic retention across repeated compactions remain unproven. Structural validators
do not make compaction lossless.

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
