# Named configuration contexts (initial declarative implementation)

Contexts are user-defined; no names are built in. Keys are stable identities and `label` is optional presentation. Select through a config overlay, for example `BCODE_CONFIG_TOML='[contexts]\nactive = "alpha"'` (supply a real newline in the environment value).

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

## Current limitations — not product completion

* Contexts currently require predeclared authentication profiles; automatic enrollment into an undeclared context profile fails closed.
* Interactive context creation/selection, scoped runtime account registration, adoption review, and durable session context selection are not implemented.
* Full launch handoff is not atomic with validation. Context switching is not yet a supported live-session operation.
* Compatibility work must address existing global names that resemble qualified names; the qualification format is not a general credential authorization boundary.
* Context-local model profiles currently use the existing setup editor, which does not yet target context-local configuration edits.

Do not describe this as complete account isolation or a completed onboarding redesign.
