# Workflow plugins and templates

## Ownership and boundaries

Workflow product behavior remains plugin-owned. A plugin may contribute typed procedural blocks through a `bcode.workflow-block/v1` service and may contribute discoverable workflow templates at manifest top level. The daemon owns validation, durable scheduling, persistence, authorization routing, and bounded inspection; it does not absorb plugin-specific workflow policy.

Disabling a plugin removes its blocks and templates from discovery. Starting a previously described template fails closed when its owner or an exact required block is unavailable.

## Declaring workflow blocks

A workflow block is declared under the owning service:

```toml
[[services]]
class        = "service"
interface_id = "bcode.workflow-block/v1"
name         = "Example Workflow Blocks"

[[services.workflow_blocks]]
plugin_id              = "bcode.example"
block_id               = "example.verify"
block_version          = 1
operation              = "example.verify"
effect                 = "read_only"
reconciliation         = "idempotent_replay"
timeout_ms              = 30000
cancellation_supported = true

[services.workflow_blocks.input]
type_name = "bcode.example.verify-request/v1"
schema = { type = "object", additionalProperties = false }

[services.workflow_blocks.output]
type_name = "bcode.example.verify-result/v1"
schema = { type = "object", additionalProperties = false }
```

The declaration also carries exact authorization and resource claims when applicable. Block identity, schemas, effect, reconciliation class, timeout, cancellation support, authorization, and resources are part of the compiled workflow definition and its digest. Registration and start resolve every block to an exact enabled declaration.

Use a procedural block for deterministic domain work already owned by a plugin: command execution or another operation with a typed request/result contract. Use a prompt node when model reasoning is necessary. A prompt may request a skill in ordinary instruction text; workflow definitions do not select skills or inherit skill model/tool policy. Do not use a prompt to reinterpret output that a typed transform or procedural owner can handle deterministically.

## Declaring workflow templates

`workflow_templates` is a versioned top-level manifest contribution. Each contribution includes:

* `contribution_version`, currently `1`;
* stable owner-local `template_id` and positive `template_version`;
* bounded title and description;
* a typed configuration schema;
* one exact declarative compiled workflow definition;
* required plugin and production-capability identities;
* renderer-neutral presentation metadata.

Discovery validates the contribution and current production admission without starting a run. The host derives identity from owner plugin, template ID/version, and the complete normalized compiled-definition digest, so topology or policy changes cannot reuse an old exact identity.

The bounded list/describe APIs retain unavailable templates with explicit diagnostics for missing plugins or unsupported capabilities. Typed start resolves the currently loaded contribution again, rejects diagnostics, validates configuration on the daemon, persists the resulting exact compiled definition, and then creates the parent-session-bound run. Skill requests, if any, are ordinary prompt text inside the definition rather than template requirements or host compilation bindings.

## Typed transforms and state envelopes

Deterministic adaptation belongs on dataflow edges through `WorkflowTransform` version 1. Expressions are finite and renderer-neutral. Supported operations are:

* `input` from exact named sources;
* `constant`;
* explicit `object` and ordered `array` construction;
* deterministic `merge` with an explicit conflict policy;
* integer `increment`;
* `default` for absent or null optional values.

Stable source names include the current output, retained workflow state, and deterministic parallel members. Expressions cannot execute code, recurse, access files or networks, or use ambiguous implicit coercion. Transform output is validated against its declared schema before persistence and again against a successor input schema before activation insertion.

A workflow state envelope is an ordinary versioned JSON-schema value owned by the template. It should contain only durable state needed by later nodes: iteration/accounting fields, exact verification evidence, repository facts, proposed commit data, and terminal decisions. Request-only prompt context, secrets, transient progress, and unbounded command output must not enter the envelope implicitly. Large output belongs in checksummed artifacts referenced by typed results.

## Mutation approval and repair-required behavior

Mutating blocks require authorization before side effects. Exact approval binds definition/version, run, node, activation, workspace snapshot, plugin/block/version/operation, capability, resource claims, reconciliation class, immutable input summary, and normalized input checksum. Approval and grant persistence complete before an activation becomes dispatchable.

Denial, expiry, stale scope, or cancellation creates no dispatch. An approval cannot authorize changed input, workspace, node, attempt, or block identity.

`repair_required` means owner acceptance occurred but the terminal external outcome cannot be proven safely. The runtime must not automatically replay that operation. Normal status and history remain bounded and non-mutating; explicit doctor/reconciliation/repair uses persisted dispatch identity, receipts, and owner evidence to resolve ambiguity. Read-only or genuinely idempotent blocks may declare `idempotent_replay` only when the owner can safely reconcile duplicate delivery.

## Runtime-authored workflows versus plugin templates

A plugin template is immutable plugin-owned discovery data. Its manifest identity, configuration
schema, requirements, and exact definition remain controlled by the plugin version that contributed
it. Describing or starting a template does not create user-owned mutable source, a draft, a published
authored revision, or an active-revision pointer.

A runtime-authored workflow is instead a user/application-owned portable
`WorkflowAuthoringDocument`. It may reference currently available plugin blocks, agents, or skills,
but it cannot embed plugin instances or take ownership of their behavior. Draft, validation,
publication, revision, preset, import/export, and execution semantics are defined in
[`runtime-workflow-authoring.md`](runtime-workflow-authoring.md).

A template can seed runtime authoring only through an explicit conversion or fork operation. That
operation materializes a complete authoring document, records source provenance, validates current
requirements, and creates a distinct logical workflow identity. Later plugin-template updates do not
silently rewrite that draft or any published authored revision. Conversely, changing an authored
workflow never mutates the contributing plugin manifest.

Disabling a plugin removes its current blocks and templates from discovery. Existing authored
revisions remain immutable but report the missing current requirement and fail closed on new starts
that need the unavailable block. Existing runs remain pinned to their exact revision and definition;
no renderer or plugin may migrate them implicitly.

## External authoring documents and composed templates

Inline manifest definitions remain supported. Large bundled product templates may instead use a
versioned contribution that references a standard `WorkflowAuthoringDocument` beneath the plugin
package and declares its expected SHA-256. Discovery canonicalizes and confines the source path,
reads it within the authoring-document bound, verifies the digest, and validates it through the same
portable authoring pipeline used for client documents. Unknown versions, path escapes, missing files,
digest mismatch, or invalid source produce diagnostics and never fall back to an older or different
template.

Instantiating either template form creates a normal mutable authored draft. It does not start the
workflow, grant authority, or create a private plugin-owned draft. Exact child-workflow dependencies
inside a template are resolved and previewed before publication; their requirements and effects are
aggregated rather than hidden behind the parent template.

The workflow plugin's composable coding product and three-level flagship workflow are specified in
[`composable-coding-workflows.md`](composable-coding-workflows.md).

## Authoring surface

The workflow plugin contributes `workflow.author`. Existing template describe/configure presents
exact template identity, requirement diagnostics, configuration JSON, and external
effect/reconciliation implications before start. Runtime authoring uses the same portable application
contracts as CLI, SDK, frontend, plugin, and generated producers; the workflow plugin may adapt those
contracts for terminal presentation but does not own draft, revision, validation, or publication
semantics.

Template start remains disabled until requirements are available and configuration is present; daemon
validation is authoritative even after local preview. Authored workflow publication and start likewise
use daemon-authoritative validation, production admission, authorization, and exact revision
selection. Presentation metadata, producer provenance, and generated prose never affect block
dispatch or permission facts.
