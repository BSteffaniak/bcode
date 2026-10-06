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

Remaining coverage includes receipt-aware summary wiring, workflow semantic history,
complete descendant/session pagination, judgement request/answer correlation and
provider retry/queue attribution. These gaps block a fully calibrated causal claim;
the introspection index is a first layer, not closure of the harness.
