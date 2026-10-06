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

Currently the harness retains trace-blob references, not their bytes. Exact model
input export requires a confined, bounded domain-owned application operation;
ordinary session artifact range access does not implicitly authorize trace paths.
A reference alone is not proof that a model input is available or complete. The
index marks `complete_model_inputs_exported=false` even when references exist.

## Controlled diagnosis

Pin the executable, provider/model, suite, fixture, acceptance criteria, resources
and scheduling seed. Preserve independent artifact checks and negative controls.
For one proposed change compare per-request observations, tool counts, stage
intervals, correction cycles, output correctness and total telemetry across paired
repetitions. Separate missing evidence from quality failures. Do not infer a causal
fix from a single noisy pass or treat tool-call overlap as useful parallel work.

Remaining coverage includes exact trace export, workflow semantic history,
complete descendant/session pagination, judgement request/answer correlation and
provider retry/queue attribution. These gaps block a fully calibrated causal claim;
the introspection index is a first layer, not closure of the harness.
