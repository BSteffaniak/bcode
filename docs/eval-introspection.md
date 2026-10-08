# Eval introspection

Every retained transcript now has adjacent `.introspection.jsonl` and
`.introspection.json` artifacts, for direct sessions and workflow child sessions.
The JSONL index preserves session identity and canonical sequence and exposes
request-build observations, provider-round timing, tool tracing, usage observations,
context occupancy and compaction boundaries. Join it to the canonical transcript
by session/sequence; use workflow inspection for execution-session provenance.
`timing.json` supplies aggregate attempt interval attribution.

The summary reports observed request boundaries and missing request-blob references.
It never reconstructs a request from later session events. Input usage can be
cumulative and duplicated; do not sum index rows. Existing canonical telemetry owns
usage accounting. Trace metadata is omitted from the index; raw transcripts and
trace payloads are sensitive diagnostic artifacts, not public frontend contracts.

## Shared-parent execution compatibility

Current workflow representation links each shared-parent dispatch to its canonical
session by exact run/node/activation/attempt identity; multiple sequential attempts
may target that same session. Active progress additionally requires the held
sequential permit's matching provenance, exact runtime-work association, ownership,
and residency. Parent session provenance is not rewritten. Inspection verifies the
explicit shared context and run workspace through bounded canonical reads.

Legacy dispatch retains its existing no-link shared-parent behavior: its historical
session-link schema permits only one dispatch per session. No schema migration or
new progress guarantee is implied for that representation. Unknown or damaged
representations fail closed, rather than falling back to legacy behavior.

## Representation-aware workflow capture

Capture follows the typed public inspection capability declaration. Current runs
that declare semantic `events` unavailable retain that boundary as `unsupported`;
the harness separately pages `workflow observations` at a fixed high-water and
captures attempt summaries. Exhaustion proves only capture of the journal's
explicit finite scalar source scope, not full semantic execution history. Disabled,
malformed, future-version, or failed observation capture remains incomplete.
Legacy event capture is unchanged. Session transcript coverage has independent
receipts; workflow completion and quality grading do not establish evidence coverage.

Current launch observation consent is derived from existing observability settings:
`persist_model_requests || persist_tool_io`. There is no scalar-journal-only setting.
Enabling either toggle also retains its declared model/tool trace payloads; it must
not be described as scalar-only instrumentation. Paired experiments must record
that configuration change and its storage/timing confound. Existing runs are not
backfilled, and cancellation requests do not certify remote termination.

## Timeout diagnosis

New repetition results can retain resolved `timeout` settings: `timeout_mode`,
`timeout_ms`, and optional `max_duration_ms`. Missing labels on historical
results mean unrecorded policy, not permission to reinterpret a prior failure.
The default mode remains `wall_clock`; only explicit `idle` selection changes
`timeout_ms` into an inactivity interval. An independent total limit remains
separate from that interval.

Diagnose idle exhaustion separately from wall-clock or total-limit exhaustion.
Idle progress must reflect substantive model, tool, or stage activity, including
descendants; polling, heartbeat traffic, and running-state observations are not
progress. Correlate retained evidence with execution provenance rather than
inferring resets from duration or observation frequency. Missing progress
coverage prevents a complete causal attribution. Idle exhaustion alone is not
evidence of remote-provider queueing or a provider failure.

These model labels and diagnostic rules do not establish that all execution
methods implement idle tracking. Runtime observation, cancellation, and result
wiring require adapter-specific verification; no live validation is implied.

## Retained evidence and current verification boundary

The first complete idle run, `remaining-20261007/runs/idle-paired`, retains
36/36 results and reports **0/36 execution passes**, not a performance comparison.
Its immutable executable SHA-256 is
`c39bda1aac8b28b0f2ed92550b91172bde01664a42fbf005199c7104cb878109`;
source HEAD and diff hash, all nine collaboration outcomes, and the failure
distribution are retained in
[Controlled eval experiments](eval-controlled-experiments.md#first-complete-idle-run-2026-10-07).
All trials resolve to idle 180000 ms without a total cap; none establishes idle
exhaustion. Failures arise in activity/child discovery, execution-session coverage,
inspection RPCs, or daemon setup. Three artifact oracles pass despite cancelled
executions. Workflow capture is incomplete in all 27 trials; 24 cancellations are
verified, three roots remain paused/unverified, and eight started direct session
lifetimes are explicitly unverified. Neither complete result inventory nor endpoint
loss proves that no actionable workers or unresolved effects remain. No foreign
artifact was operated to resolve those lifetimes.

The historical `wait-fixed-full-paired` report remains separate. It reports
27/36 successful executions, with seven of its nine deadline-failed collaboration
trials nevertheless passing the independent artifact oracle. Its all-nine
collaboration table is preserved in
[Historical full-run evidence](eval-controlled-experiments.md#historical-full-run-evidence-reviewed-2026-10-07).
Artifact correctness, completion judgement, deadline enforcement, cancellation
request, terminal cancellation, and capture availability are separate facts.
The historical separable/1 root `inspection.json` records terminal `cancelled`; eight
other failures retain inspection timeout errors and do not establish terminality.
All 27 paired comparisons are marked evidence-incomplete, with no eligible speed
samples. Complete result inventory is not complete causal capture.

Post-execution capture contributes to repetition wall time without extending the
execution allowance. For example, the older clean-run auto/handoff/3 result
records 172937 ms workflow execution, 120000 ms capture, and 299309 ms repetition
wall time. Its root inspection records terminal `failed`, not timeout; iteration-2
`loop.evaluation` output keeps `condition_met: false` because inspectable evidence
that Stage 2 consumed normalized.json was missing, despite correct file contents.
This establishes the retained completion decision, not proof that consumption did
not occur. Capture overhead alone does not establish token inflation.

Current source uses comparable substantive progress marks and fails closed on
unavailable coverage. The first idle run's infrastructure failures must not be
attributed to historical provider or completion decisions. Subsequent corrections
separate transaction-local workflow reads, release temporary session DB handles,
bound initial-baseline acquisition, and retry normalized stale projections without
counting retries as progress. These source mechanisms are not hash-matched proof
that every c39b RPC timeout had the same cause. Coordinator-retained logs currently
report 13 session-child, seven workflow-activity, and three workflow-store tests
passing; regression logs and clippy are coordinator-owned and are not live
full36 success evidence. The corrected immutable 8b916 pilot and corrected full36
rerun remain separate validation; preserve the original c39b results unchanged.

The existing offline commands below are reproducible validation entry points;
their presence is not evidence they ran against today's source. Historical closure
logs also retain a passing `attempt_timing_transport_tests::timing_transport_requires_explicit_acceptance`
test. Neither that test nor the later critical2 timing report proves complete
provider-round, remote-queue, tool, or workflow-wait attribution.

## Judgement diagnostics

The loop plugin accepts an explicit sensitive-capture setting:

```toml
[plugins.config."bcode.loop"]
persist_judgement_diagnostics = true
```

It defaults to false and leaves loop input/output schemas unchanged. When enabled,
the judgement block writes bounded version-1 generic invocation artifacts for its
normalized iteration input, outgoing normalized request, validated normalized response,
and decision outcome. The input records evaluator kind and input role without guessing
upstream creation provenance.
The outcome includes threshold, evaluator result, actual fallback selection and
capture receipts. These are not provider-wire responses or hidden reasoning.
Agent-only loops capture their normalized input and deterministic evidence-safeguard
outcome under the same explicit consent, labeling provider judgement not applicable.
Capture failures do not change completion decisions. Generic workflow-artifact
reference discovery/export uses `workflow diagnostic-artifacts` and
`workflow read-diagnostic-artifact`, bound to an exact immutable dispatch receipt.
The harness exports those bytes in bounded, digest-verified pages. A daemon loss
before receipt persistence remains explicitly unavailable; files are not scanned
to invent provenance. Legacy receipt lookup is unavailable. These diagnostics
certify retained bytes, not full original payload coverage or provider-wire capture.

Workflow capture emits `capture-completeness.json`, separating public events,
attempts, judgement and provider-wire availability. Unsupported history stays
explicit; snapshots are not a substitute for a semantic journal. Audit an existing
controlled run without rewriting it:

```sh
python3 fixtures/evals/introspection/verify_artifacts.py /absolute/run --controlled-audit
```

## Finite transactional observations

`workflow observations --run-id <id> --limit 100` returns a versioned page with
exact source coverage, exclusions, optional recorded timestamps and an exclusive
sequence cursor. Pin `--through-sequence` from the first page on subsequent
requests. This is a bounded diagnostic scan, not a durably resumable subscription
or reconstructed semantic history. Sources without direct run scope remain
explicitly excluded; unknown timestamps are never guessed.

The additive observation extension leaves representation v30 and the outer store
contract unchanged. Coverage starts atomically before new-run admission; older
runs are never backfilled. Independent writers emit observations in their source
transaction once coverage is enabled. Rollback removes both writes. Eval retains
`workflow-observations-*.json`; unavailable coverage is a capture failure rather
than evidence that no transitions occurred.

## Isolation and cleanup

Isolated trials override both `BCODE_STATE_DIR` and `BCODE_SESSION_STORE_DIR`;
an inherited session-store override must not expose unrelated sessions. The harness
restores the caller's environment and process selection after each trial.
Represented terminal workflows release their exact local runtime registration
under verified local authority, without taking over foreign work for cleanup.
Idle-stop acknowledgement, endpoint disappearance and verified process exit are
distinct evidence; cleanup records must not treat one as another.

## Exact inputs

Existing domain-owned observability supports explicit request-time capture through
an isolated trial configuration overlay:

```toml
[observability]
persist_model_requests = true
persist_tool_io = true
```

This persists sensitive request content in the subject daemon's trace store.
Credential fields are redacted by the runtime, but user text and tool outputs can
still contain secrets. Enable only intentionally for controlled fixtures. Do not
include credentials in suite manifests. Bounded captures can be truncated and that
status must remain visible.

The harness exports explicitly captured trace bytes through
`client.session_trace_blob_range`, a confined bounded application operation, into
adjacent `.captures/` files with `.captures.json` receipts
(`bcode.eval.trace_exports.v1`). Receipts distinguish `exported`, `unavailable` and
`verification_failed` and preserve completeness/redaction. Ordinary session
artifact range access does not authorize trace paths. References alone do not
prove an input is available or complete.

## Correlated timeline and capture manifest (index version 2)

Version 2 is an eval-derived artifact format, not a canonical session schema
migration. Consumers must check `schema_version`; version 1 lacks correlation and
capture-manifest fields. Unknown versions must not be interpreted as version 2.
Canonical transcripts remain unchanged.

Every projected row has session/sequence/timestamp and a `correlation` object.
Only identities actually persisted on that event are populated. Context events
carry request/turn/round identities; usage carries request/turn; tool traces carry
tool-call/turn; provider rounds carry provider-turn/round. Request-build traces
currently lack request IDs. Ordering within a turn does **not** establish an exact
request join, nor does a provider-turn ID equal a host request ID. Null IDs stay
null. Turn boundaries, cancellation, canonical tool requests/results and permission
boundaries appear alongside trace observations. Tool result summaries contain
counts, not model-visible text or plugin-owned metadata.

Duplicate delivery of `(session_id, sequence)` is ignored after its first retained
observation. `selected_usage` calls the same cumulative-request selector as eval
telemetry, scoped by session; later snapshots cannot reopen terminal usage. Missing
request IDs remain independent observations, not guessed requests. Raw usage rows
are observations, never additive charges. Event order is the caller's retained
canonical order; this module does not read, repair or replay session storage.

The summary's `capture_manifest` includes request/argument/output boundaries even
when capture is absent. Each entry reports `not_captured`,
`referenced_unverified`, or `malformed_reference`, plus recorded completeness and
redaction when present. Retained/truncated content is never described as complete.
Digest-shape validation does not verify bytes. Summaries omit trace paths,
provider metadata, auth/compatibility details, error prose, retry messages and
arbitrary trace detail. Sensitive raw transcripts and explicitly exported blobs
remain separate controlled artifacts. A summary is not a credential scrubber for
arbitrary user-chosen identifiers.

### Exact-export integration hook

`introspection::referenced_blobs(events: &[SessionEvent]) -> Vec<CaptureReference>`
returns unique capture references in retained order. Fields are `session_id`,
`sequence`, optional `turn_id` and `tool_call_id`, `kind` (`model_request`,
`tool_arguments`, `tool_output`), and typed `blob: TraceBlobRef`. References are
sensitive routing data: `blob.path` is an opaque application locator, not a grant
to read a private trace filesystem. The caller passes them to the bounded,
confined domain-owned application trace-export API. This function performs no IO.

`introspection::summary_with_exports(events: &[SessionEvent], receipts: &Value)`
merges the coordinator's verified `bcode.eval.trace_exports.v1` receipt document.
Call it after export instead of `summary(events)` when writing the adjacent summary.
It checks schema, canonical identity, status, digest/length and recorded
completeness/redaction; unknown, missing, conflicting and incomplete receipts fail
closed. It emits normalized outcomes without filenames or digests. The ordinary
`summary(events)` keeps `complete_model_inputs_exported=false`; the receipt-aware
summary sets it only when every observed request-build boundary has a complete
verified export, and at least one request was observed. This flag covers retained
observations only, not full-session coverage or absence of redaction. Model settings
are shown only where request-time trace fields exist; full settings, snapshots and
exact inputs require verified captured bytes. Nothing is reconstructed from later
history. Workflow handoffs require workflow semantic history plus execution-session
provenance; provider queue time and retry waits are not inferred from round duration.

## Controlled diagnosis

Pin the executable, provider/model, suite, fixture, acceptance criteria, resources
and scheduling seed. Preserve independent artifact checks and negative controls.
For one proposed change compare per-request observations, tool counts, stage
intervals, correction cycles, output correctness and total telemetry across paired
repetitions. Separate missing evidence from quality failures. Do not infer a causal
fix from a single noisy pass or treat tool-call overlap as useful parallel work.

### Observation allowances and paired reports

Workflow post-execution capture has an independent allowance, selected by suite
metadata `capture_allowance_ms` (positive milliseconds; default 120000). It does
not extend task execution. Exhaustion retains partial evidence and the observed
outcome. Reports retain capture allowance, capture duration and exhaustion; direct
execution timing also excludes transcript export overhead.

Paired report schema v3 uses authored case/repetition blocks. Scheduled, observed,
evaluable, missing, infrastructure-failed and duplicate outcomes remain distinct.
All-attempt success rates use scheduled denominators; conditional-evaluable rates
are named separately. Genuine terminal workflow failures remain quality outcomes.
Missing or conflicting capture evidence and invalid latency measurements cannot
qualify a speed sample. Intervals are descriptive, not noninferiority claims.

Diagnostic discovery distinguishes unavailable entries and omitted reference
counts. Zero omitted references certifies only no collection overflow; historical
unknown coverage or overflow never certifies complete capture. Direct cleanup uses
require-running clients, own-session cancellation and isolated idle-only shutdown.
Endpoint disappearance is evidence, not proof that execution lifetime ended.

Remaining coverage includes receipt-aware summary wiring, workflow semantic history,
complete descendant/session pagination, judgement request/answer correlation and
provider retry/queue attribution. These gaps block a fully calibrated causal claim;
the introspection index is a first layer, not closure of the harness.
