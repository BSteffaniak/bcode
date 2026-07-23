# Tool Runtime Contract Ownership

This document defines package ownership and dependency direction for Bcode's neutral tool runtime contracts. Domain and platform packages may implement these contracts, but canonical orchestration must not depend on those implementations.

## Package ownership

### `bcode_tool`

Owns serializable, transport-free tool protocol models:

* invocation identity and arguments
* opaque preparation host context
* prepared invocation descriptors
* provider-batch execution options
* authorization facts
* lifecycle and contribution envelopes
* exchange, input, nested-service, and artifact request/result envelopes

`bcode_tool` describes mechanisms only. It must not depend on plugin loading, sessions, IPC, TUI, web rendering, provider implementations, filesystem clients, or process execution.

### `bcode_agent_runtime`

Owns orchestration behavior:

* monotonic turn-generation allocation
* turn and invocation scopes
* final scope acceptance checks
* cancellation registration and fan-out
* complete-batch preparation and authorization
* provider-batch bounded scheduling
* ordered result collection
* host capability traits for exchanges, inputs, services, and artifacts

It may depend on protocol/model crates, but it must not depend on the server, IPC, plugin host, session implementation, TUI, web renderer, or bundled plugins.

### `bcode_model`

Owns provider-neutral model request/response types. Provider-visible tool definitions contain only name, description, and input schema. Scheduling, authorization, presentation, and host transport metadata must not cross this boundary.

### Domain plugins and direct tools

Own domain semantics and produce opaque contracts understood by matching adapters:

* authorization fact schemas
* contribution and exchange schemas
* cancellation implementation
* final model-visible tool results

Shell, filesystem, web, question, Vim, document, and other semantics stay in their owning implementation or policy adapter.

### Host adapters

Server, SDK, static-plugin, dynamic-plugin, remote, TUI, web, and headless hosts implement neutral runtime traits. They may persist, route, render, or resolve opaque envelopes, but they must not require core orchestration to decode domain payloads.

### Provider-batch parallel intent

One provider tool-call batch is the model's declaration that its calls may overlap. After complete-batch authorization, approved calls execute concurrently without a default limit, or up to an explicitly configured positive host bound. If one call depends on another, the provider must emit it in a later tool round after receiving the earlier result. Core and domain plugins do not infer command, path, repository, or resource conflicts for scheduling.

A host may explicitly disable parallel execution, and a non-reentrant adapter may serialize internally as a mechanical implementation constraint. Neither case introduces tool-domain policy into canonical orchestration.

## Contribution placement ownership

A producer must wrap visible contributions in `ToolContributionEnvelope` and explicitly select
request, progress, result, supplemental, or hidden placement. The host validates producer and
invocation identity, transports transient envelopes live-only, and persists durable envelopes through
the append-only placed-contribution session event. Legacy unplaced contributions remain accepted for
compatibility but default to hidden presentation.

Placement selects semantic composition only; it never selects a renderer. Request, progress, and
result contribution slots coexist; result placement does not replace request context or supersede the
canonical semantic `ToolInvocationResultRecorded` result card. Plugins continue to own
payload schemas and adapters, `SessionView` owns stable slot identity and ordering, and each renderer
owns native styling. Renderers may expose raw contribution payloads only on an explicit diagnostic or
developer surface, never as a normal transcript fallback.

## Artifact write contract

Artifact ABI v1 is a bounded atomic write rather than an allocate/write/finalize protocol. One `ToolArtifactWriteRequest` carries the complete bytes, content type, producer metadata, invocation identity, and invocation-local artifact ID. The host validates identity and size before publication and returns exactly one terminal `ToolArtifactWriteResolution`.

This shape is intentional:

* no incomplete allocation survives cancellation or plugin failure
* the host chooses and enforces its byte bound
* duplicate invocation-local IDs cannot overwrite prior artifacts
* host sinks publish transactionally and return opaque references
* larger or streaming artifacts require a future versioned contract rather than unbounded v1 buffering

Allocation and finalize operations are therefore not part of the stable bounded v1 API.

## Detached cleanup completion

Local cancellation changes active runtime work to `Cancelling` before cleanup begins. Detached cleanup completion does not remove or finish that work item; only termination of the owning operation does. Cleanup completion and failure are emitted as diagnostic tracing with session/work or provider/turn identity. Failures never reverse local cancellation and are never returned through the cancellation acknowledgement.

This separation keeps runtime-work state truthful: `Cancelling` means the owning operation has not yet reported terminal completion, regardless of whether its best-effort cleanup signal succeeded.

## Dependency direction

Allowed direction:

```text
domain tools/plugins ─┐
platform/host adapters ├─> bcode_agent_runtime ─> bcode_tool
provider adapters ─────┘          │                 │
                                  └──────────────> bcode_model
```

Forbidden direction includes:

* `bcode_tool` depending on runtime or product packages
* `bcode_agent_runtime` depending on server, plugin host, session implementation, IPC, TUI, web, or bundled plugins
* `bcode_model` depending on tool policy, provider-batch execution policy, or renderer metadata
* domain plugins requiring new scheduler branches for tool, command, path, or resource semantics
* tools selecting a concrete renderer or persistence representation

## Compatibility boundary

Legacy executor adaptation may reconstruct old transport requests only inside explicitly named compatibility adapters. Canonical preparation and invocation APIs remain transport-free. Compatibility adapters must be deleted when their callers migrate; they are not valid extension points for new behavior.
