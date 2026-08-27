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
