# Model-specific context compaction

Bcode can apply automatic compaction policy to an effective concrete model ID. Model-specific values inherit unspecified fields from `[model.compaction]`.

```toml
[model.compaction]
mode = "auto"
backend = "auto"
proactive_threshold_percent = 90
keep_recent_tokens = 20000

[model.compaction.models."gpt-5.6-sol"]
provider_plugin_id = "bcode.amazon-bedrock"
mode = "proactive_and_overflow"
proactive_threshold_tokens = 268000
```

Use the provider and effective model IDs reported by Bcode. `provider_plugin_id` is optional, but prevents an override from matching an identically named model from another provider.

An absolute `proactive_threshold_tokens` takes precedence over a model-specific or global percentage. It measures Bcode's estimate of the complete candidate input request. The effective trigger is capped at the model's safe input capacity after output reserve and safety margin. Because provider billing token accounting can differ, configure a buffer below a hard pricing boundary—for example, `268000` for a `272000` boundary.

Defining a threshold does not enable proactive compaction by itself. Set the model override's `mode` to `proactive` or `proactive_and_overflow`. `backend = "auto"` prefers provider-native compaction where supported and otherwise uses local compaction.

Recent-context retention preserves complete tool exchanges, not an entire long-running user turn. Once all calls in an exchange have results, that exchange and its post-tool continuation can be summarized before the next exchange. Parallel batches and unresolved calls remain together; `keep_recent_tokens` retains a structurally complete recent tail.

## Continuation safety

Local synthesis processes chronological, attributed source fragments rather than a truncated transcript prefix. A bounded operation selects complete conversational units; material outside that selection remains in canonical context for a later operation. Oversized user directives are fragmented without dropping their endings. Tool results retain bounded, untrusted model-context projections, not a promise to preserve every artifact byte.

Summaries separate derived continuation facts from current user scope and its canonical source references. New user corrections may supersede conflicting earlier scope, but assistant interpretations and tool reports cannot authorize work or revoke user restrictions. Generated summaries are projected as assistant recollection, not system instructions. This applies to existing textual summaries as well as new structured summaries; legacy attribution is not retroactively invented.

Synthesis has bounded input, output, and repair attempts. Invalid, incomplete, or oversized synthesis does not publish an advancing boundary. Cancellation is checked during processing and again at marker commit. Summary output is validated rather than clipped mid-record. Compaction remains lossy: behavioral and live-provider evaluation are needed to assess semantic retention beyond structural coverage.

Explicit native recompaction includes the previous marker, allowing compatible opaque context or its portable fallback to carry earlier work into the next snapshot. Provider/model/authentication/format compatibility checks remain target-specific. Request-only guidance must not be copied into portable recollection from assembled provider requests.

## Reconstructing an already stale summary

Ordinary compaction cannot recover evidence already omitted by an earlier summary. Explicitly use `session compact ID --rebuild-summary --expected-marker SEQ` to reconstruct the latest selected marker from canonical source history. The operation uses the owning session's serialized command path and replaces the summary at the same covered boundary, leaving the uncompacted tail intact. Pending submissions and old summaries are not source evidence.

Recovery is separately bounded by `--rebuild-max-events`, `--rebuild-max-pages`, `--rebuild-max-decoded-bytes`, `--rebuild-max-provider-requests`, and `--rebuild-timeout-ms`. Invalid or exhausted allowances, changed source generations, cancellation, missing canonical evidence, or invalid synthesis leave the existing marker unchanged. It may call the selected model; it is neither an automatic normal-read repair nor a guarantee of semantic retention. Inspect the current marker before requesting recovery; conflicts require an explicit new selection, not a silent retry.
