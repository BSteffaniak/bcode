# Model prompt profiles

Bcode resolves model-scoped system-prompt and tool-description text through the versioned
`bcode.prompt-profile/v1` plugin interface. The bundled `bcode.prompt-profile` plugin is enabled
with the other bundled plugins and remains independently disableable through normal plugin
selection.

## Matching and precedence

Profiles use exact host-supplied model facts. From lowest to highest precedence, layers are:

1. `prompt_profile.default`
2. `prompt_profile.provider.<plugin-id>`
3. `prompt_profile.family.<catalog-family>`
4. `prompt_profile.catalog_entry.<catalog-entry-id>`
5. `prompt_profile.model.<effective-model-id>`

Bcode's model catalog supplies family, catalog-entry, and API-surface identity. The plugin does not
infer identity from model-ID substrings. A family such as `claude` is shared by every Anthropic
generation in the catalog (Opus, Sonnet, Haiku, Fable, Mythos, and the `anthropic.claude` catch-all
entry), so a family layer is the right scope for guidance that applies to all of them, while
`catalog_entry` layers target a single generation.

Each layer may append, prepend, or replace stable system-prompt text and model-facing tool
descriptions. These are presentation changes only: tool names, schemas, authorization facts,
dispatch, and persisted execution outcomes are unchanged.

## Bundled Claude profile

The bundled profile appends guidance to the `shell.run` tool description telling Claude models not
to pipe command output through `head`, `tail`, `sed`, or similar filters merely to shorten it. Bcode
already bounds model-visible tool output while retaining the complete output for the user;
self-truncation only hides useful output. The profile does not touch the system prompt: the
guidance is about one tool, so it travels with that tool and is absent when the tool is not offered.

The default applies whenever catalog resolution reports `family = "claude"`, which covers every
Anthropic entry in the Bedrock catalog including region-prefixed inference-profile IDs. Models
accessed through an unmapped gateway resolve without a family and can be targeted explicitly with
`prompt_profile.model.<effective-model-id>`.

Set `system_prompt.sections.model_profile = false` to disable all profile application for coding
turns, or disable the `bcode.prompt-profile` plugin to remove all bundled behavior without affecting
unrelated Bcode capabilities. A single shipped profile can be disabled independently:

```toml
[prompt_profile.bundled]
disabled = ["anthropic-claude-output-preservation"]
```

Bundled profile documents live under `plugins/prompt-profile-plugin/profiles/`; the Claude prompt
text and target are defined in `anthropic-claude-output-preservation.toml`, separately from runtime
code. A bundled document targets either `[target] family = "<catalog-family>"` or
`[target] catalog_entry = "<catalog-entry-id>"`.

Prompt profiles are applied to ordinary coding turns. Utility prompts such as invariant selection,
compaction, title generation, or other internal model requests are intentionally outside this
interface.
