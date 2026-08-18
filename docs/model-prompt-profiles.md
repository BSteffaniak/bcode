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
infer identity from model-ID substrings. A family such as `claude` is shared by Sonnet, Haiku, and
multiple Opus generations, so it cannot target Opus 5 alone. The bundled default therefore uses
`catalog_entry."anthropic.claude-opus-5"`.

Each layer may append, prepend, or replace stable system-prompt text and model-facing tool
descriptions. These are presentation changes only: tool names, schemas, authorization facts,
dispatch, and persisted execution outcomes are unchanged.

## Bundled Opus 5 profile

The bundled profile tells Opus 5 not to pipe tool output through `head`, `tail`, `sed`, or similar
commands merely to shorten it. Bcode already bounds model-visible tool output while retaining the
complete output for the user; self-truncation only hides useful output.

The default applies only when catalog resolution identifies
`anthropic.claude-opus-5`. Models accessed through an unmapped gateway can be targeted explicitly
with `prompt_profile.model.<effective-model-id>`.

Set `system_prompt.sections.model_profile = false` to disable all profile application for coding
turns, or disable the `bcode.prompt-profile` plugin to remove the bundled behavior without affecting
unrelated Bcode capabilities.

Prompt profiles are applied to ordinary coding turns. Utility prompts such as invariant selection,
compaction, title generation, or other internal model requests are intentionally outside this
interface.
