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

Use a procedural block for deterministic domain work already owned by a plugin: command execution, repository inspection, committing, or another operation with a typed request/result contract. Use an agent node when model reasoning is necessary. Use a skill selection on an agent node when reusable instructions or model/tool policy are required. Do not use an agent to reinterpret output that a typed transform or procedural owner can handle deterministically.

## Declaring workflow templates

`workflow_templates` is a versioned top-level manifest contribution. Each contribution includes:

* `contribution_version`, currently `1`;
* stable owner-local `template_id` and positive `template_version`;
* bounded title and description;
* a typed configuration schema;
* one exact declarative compiled workflow definition;
* required plugin, skill, and production-capability identities;
* renderer-neutral presentation metadata.

Discovery validates the contribution and current production admission without starting a run. Template identity is derived from owner plugin, template ID/version, and the complete normalized compiled-definition digest, so topology or policy changes cannot reuse an old exact identity.

The bounded list/describe APIs retain unavailable templates with explicit diagnostics for missing plugins, missing skills, or unsupported capabilities. Typed start resolves the currently loaded contribution again, rejects diagnostics, validates configuration on the daemon, persists the exact compiled definition, and then creates the parent-session-bound run.

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

## Authoring surface

The workflow plugin contributes `workflow.author`. Template describe/configure presents exact identity, requirement diagnostics, configuration JSON, and external effect/reconciliation implications before start. Start remains disabled until requirements are available and configuration is present; daemon validation is authoritative even after local preview.
