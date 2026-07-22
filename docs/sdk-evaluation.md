# SDK evaluation APIs

Bcode's provider-independent scoring surface is opt-in:

```toml
[dependencies]
bcode = { version = "0.0.1-alpha.0", default-features = false, features = ["evaluation"] }
```

Without `evaluation`, the high-level `bcode` crate does not depend on `bcode_eval`, and
`bcode::evaluation` is unavailable. This keeps evaluation runner, reporting, and client dependencies
out of lean model/agent applications.

## Subjects and response adapters

`SdkEvalSubject` is a serializable provider-independent scoring input containing:

* final output text;
* an optional decoded structured value;
* ordered application-defined tool traces;
* ordered application-defined agent steps;
* measured latency;
* numeric provider/application usage such as token or cost measurements; and
* arbitrary application metadata for custom criteria.

`bcode::evaluation::subject_from_response` adapts a complete `GenerateTextResponse`: every public
`GenerationStep` is serialized in order, tool-result steps become the tool trace, latency is copied,
and provider-reported token fields become usage measurements. Missing usage remains absent and
causes a usage criterion to return a typed missing-field error rather than silently estimating.
`subject_from_structured_response` additionally serializes a caller's decoded structured value.

## Criteria and reports

`SdkEvalCriterion` is the application extension contract. Criteria receive the complete subject and
return a normalized `SdkCriterionScore` with a stable name, zero-to-one score, pass decision,
measurements, and safe diagnostics. Invalid, non-finite, and out-of-range scores are rejected.
Criteria execute in registration order through `SdkEvaluator`; its report preserves that order,
passes only when every criterion passes, and computes the arithmetic mean. An evaluator with no
criteria passes with score zero rather than inventing evidence.

Bundled provider-independent criteria cover output substrings, exact structured values, tool-trace
count, agent-step count, latency bounds, and named usage bounds. Applications can implement custom
criteria for semantic quality, domain policy, costs, or any metadata they explicitly attach.

## Reproducible runs and plugin integration

`SdkEvaluator::run` scores ordered `SdkEvalCase` values. Every case requires explicit dataset ID,
dataset version/digest, case ID, model ID, and configuration identity; provider ID and additional
provenance such as git revision or prompt version are optional but preserved. Duplicate case IDs and
incomplete provenance fail before scoring.

Each run records versioned evaluator identity/version metadata, ordered case results, and a SHA-256
fingerprint over the complete evaluator configuration, ordered provenance, and subjects. The
application-assigned run ID and criterion results are deliberately not fingerprinted, so identical
inputs/scoring configuration produce identical fingerprints across reruns while changed data,
model/config/evaluator provenance, ordering, or subject material changes identity.

`SdkEvalObserver` receives ordered run/case start and finish events with provenance, pass state,
scores, and final fingerprint. This keeps observability application-owned without imposing a
telemetry implementation. `write_sdk_eval_run` atomically writes `sdk-eval.json`, and
`load_sdk_eval_run` validates its schema.

The bundled eval plugin declares the versioned `bcode.eval.sdk-artifact/v1` service. Its typed
`load_sdk_eval_run` operation loads these artifacts for plugin-owned visualization/integration; the
SDK remains independently usable without enabling or loading the plugin.

This scoring layer does not invoke providers, mutate sessions, or require the TUI or daemon. The
existing `bcode_eval` suite runner remains responsible for command-oriented datasets, repetitions,
comparisons, and its legacy run visualization; both paths share the eval package and bundled plugin
rather than creating a parallel product-specific evaluation crate.
