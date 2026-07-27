# Configurable Limit Inventory

This inventory classifies Bcode operational limits so hardcoded defaults do not silently become unchangeable policy.

## Conventions

* Trusted authoritative prompt inputs use optional positive limits; omission means unlimited.
* Generated or untrusted input, persisted previews, resource reads, protocol payloads, collection cardinality, and algorithmic work remain bounded by default.
* Bcode-chosen bounded defaults should be configurable in the domain that owns them.
* External format, platform, or protocol constraints may remain hard limits when configuration cannot change the underlying constraint.
* Truncation, rejection, eviction, or algorithm fallback should be visible or documented.

## Prompt and skill limits

| Limit | Owner | Default | Classification | Behavior |
| --- | --- | ---: | --- | --- |
| `system_prompt.repository_instructions_max_chars` | Prompt assembly | Unlimited | Optional prompt limit | Truncates `AGENTS.md` only when configured. |
| `system_prompt.repository_invariants_max_chars` | Prompt assembly | Unlimited | Optional prompt limit | Truncates `INVARIANTS.md` only when configured. |
| `system_prompt.git_status_max_chars` | Prompt assembly | 4,000 chars | Configurable bounded default | Truncates generated Git status with a visible marker. |
| `skills.max_context_bytes` | Skills | Unlimited | Optional prompt limit | Truncates model-visible skill context only when configured. |
| `skills.prompt.max_bytes` | Skills | Unlimited | Optional prompt limit | Truncates the available-skill catalog only when configured. |
| `skills.prompt.max_description_chars` | Skills | Unlimited | Optional prompt limit | Truncates catalog descriptions only when configured. |
| `skills.max_skill_file_bytes` | Skills | 256 KiB | Configurable bounded default | Rejects oversized skill definition files before reading them. |
| `skills.preview_max_chars` | Skills | 2,000 chars | Configurable bounded default | Truncates persisted transcript previews, not model context. |
| `model.tool_output.context_chars` | Model context | 4,000 chars | Configurable bounded default | Bounds normal tool-result projection. |
| `model.tool_output.fallback_argument_chars` | Model context | 6,000 chars | Configurable bounded default | Bounds plain-text fallback for malformed or duplicate historical tool calls. |

## Follow-up audit groups

The following groups should be reviewed in focused domain changes rather than converted indiscriminately:

* **TUI presentation and algorithms:** dialog sizes, inline rows, diff LCS cells, intraline graphemes, preview lengths, and pending render buffers.
* **Server protocol and live state:** adapter counts, identifier lengths, artifact ranges, active contribution bytes/counts, tool request drafts, and per-client live buffers.
* **Session persistence:** durable event bytes, provenance identifier lengths, projection/history page limits, and artifact reads.
* **Plugins:** document/OCR/fetch bytes, shell terminal output and recording frames, repository file reads, Vim playback/diff limits, eval traversal limits, and plugin-specific AI context.
* **Compaction and model context:** summary/event content bounds and carried context limits.

For each candidate, record the owning domain, trust level, default, failure behavior, whether full source remains available, existing configuration, and whether the limit is externally imposed. Convert Bcode-chosen defaults to domain-owned configuration when changing the value is safe and meaningful; document genuine hard constraints next to their definitions.
