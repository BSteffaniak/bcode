# Named configuration contexts (initial declarative implementation)

Contexts are user-defined; no names are built in. Keys are stable identities and `label` is optional presentation. Select for one invocation with `bcode --context alpha` (also accepted after subcommands). This does not write configuration. In onboarding, `N` opens reviewed creation of an empty context (stable ID and display label), and `o` opens a picker for existing contexts. Up/Down selects; Enter reviews and then confirms the destination-file edit. Escape cancels without saving. Creation does not select the context or copy credentials. Higher-priority command-line selection still wins.

```toml
[contexts]
active = "alpha"

[contexts.entries.alpha]
label = "Primary account"

[contexts.entries.alpha.model]
profile = "fast"

[contexts.entries.alpha.model.profiles.fast]
provider_plugin_id = "bcode.openai-compatible"
model_id = "your-catalog-model-id"
auth_profile = "openai"

[contexts.entries.alpha.auth.profiles.openai]
backend = "sshenv"
provider_id = "openai"
owner_plugin_id = "bcode.openai-compatible"
scheme = "chatgpt"
```

A selected context replaces global model/auth configuration rather than inheriting credentials. Account and pool references are qualified with the context identity. Default sshenv storage-profile names are qualified too. An explicit `settings.profile` is an intentional reference to existing storage; use it only when sharing/adopting that credential destination is intended. No credentials are copied or migrated.

Resolved context identity crosses effective-config transport. Model profile names remain local. Local account names supplied to auth lookup resolve within the selected context; global default provider bindings are not consulted. Missing contexts fail rather than selecting another context. Existing configurations without `contexts` retain their behavior.

## Discovered model selection

In onboarding, `M` discovers models using the selected model profile's account, the context's explicitly selected account, or its sole declared account. Preparation uses one configuration snapshot without reloading ambient layers; results are rejected if configuration changes during discovery. The account's registered plugin supplies the catalog through the application client. Up/Down selects a returned model; Enter reviews and confirms an atomic context-local provider/model/account edit. No global model default is changed. Missing or ambiguous account selection is reported instead of guessing. A dedicated multi-account discovery picker is not yet implemented; configure the account selection or use an existing model profile through `m`.

## Current limitations — not product completion

* Context account enrollment uses an explicit reviewed declaration before sign-in. The declaration does not select a model or provider binding. Automatic unscoped runtime registration remains prohibited for contexts.
* Scoped runtime account registration, adoption review, and durable session context selection are not implemented.
* Full launch handoff is not atomic with validation. Context switching is not yet a supported live-session operation.
* Compatibility work must address existing global names that resemble qualified names; the qualification format is not a general credential authorization boundary.
* Context-local model-profile selection now uses a reviewed context-targeted edit. The destination file remains explicit; higher-priority overrides may still win.

Do not describe this as complete account isolation or a completed onboarding redesign.
