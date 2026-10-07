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
