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

## Primary presentation update ownership

A tool invocation owns one stable primary transcript presentation identity. A producer updates its
current opaque payload through an invocation-scoped handle; it does not create distinct live and
final objects or coordinate request/progress/result promotion. The host validates invocation and
producer identity, generation, monotonic revision, payload bounds, retention, cancellation, and
terminal dominance. Plugins continue to own payload schemas, artifact references, and renderer
adapters.

Primary updates use **retain latest** when output should remain in history: the host keeps one
bounded current value and checkpoints only the latest accepted value at closure. **Active only** is
explicit and limited to sensitive or intentionally ephemeral state. Independently meaningful
supporting output may use stable supplemental identities, but supplemental output must not recreate
phase-based primary cards.

Closure and update delivery share one ordered boundary:

```text
stop producer acceptance -> flush accepted updates -> apply latest value
-> record terminal outcome/timing/checkpoint -> close scope
```

Closed scope state is absorbing. Delayed, duplicate, stale, or otherwise late updates cannot reopen
the invocation, restart timing, or replace retained output. Generic runtime and renderers never infer
lifecycle or retention from tool names, payload schemas, placement, or a generic `streaming` flag.

`ToolContributionEnvelope` placement remains supported while existing producers and historical
sessions migrate. Historical placed events are compatibility inputs: chronological projection adapts
the latest compatible primary contribution into the invocation's current item, preserves explicit
supplementals independently, and keeps hidden/unplaced payloads out of normal transcript UI. New
primary APIs must not require request, progress, or result slot selection.

Manifests advertise adapter schemas and versions, not lifecycle or retention policy. Renderers choose
compatible native adapters and own styling; they do not resolve competing semantic sources. Raw
payloads remain available only through explicit diagnostics and never become a normal transcript
fallback.

## Artifact write contract

Artifact ABI v1 is a bounded atomic write rather than an allocate/write/finalize protocol. One `ToolArtifactWriteRequest` carries the complete bytes, content type, producer metadata, invocation identity, and invocation-local artifact ID. The host validates identity and size before publication and returns exactly one terminal `ToolArtifactWriteResolution`.

This shape is intentional:

* no incomplete allocation survives cancellation or plugin failure
* the host chooses and enforces its byte bound
* duplicate invocation-local IDs cannot overwrite prior artifacts
* host sinks publish transactionally and return opaque references
* larger or streaming artifacts require a future versioned contract rather than unbounded v1 buffering

Allocation and finalize operations are therefore not part of the stable bounded v1 API.

### High-rate transient progress

Execution progress is plugin-owned, live-only presentation state. Plugins publish it as placed
`Progress` contribution envelopes with transient persistence and monotonic sequence identity.
High-rate schemas must support bounded latest-state materialization: each accepted `Upsert` must be
independently renderable from one bounded envelope, and any `Append` schema must let the host retain
or synthesize one bounded checkpoint without preserving append history. A schema that requires
replaying every prior append is not valid for high-rate progress.

The scoped plugin SDK publisher is the default extension point because it fixes invocation,
contribution, producer, schema, placement, persistence, sequence, host byte limits, cadence,
cancellation, and terminal removal. The low-level envelope API remains available for advanced
bounded schemas, but server validation and accounting remain authoritative.

Terminal removal dominates its sequence: duplicate, stale, and same-generation post-terminal
updates cannot recreate state. A later monotonic sequence may begin a new active generation. The
server retains only the current bounded envelope for attach/resynchronization and never writes
these updates to durable history.

## Detached cleanup completion

Local cancellation changes active runtime work to `Cancelling` before cleanup begins. Detached cleanup completion does not remove or finish that work item; only termination of the owning operation does. Cleanup completion and failure are emitted as diagnostic tracing with session/work or provider/turn identity. Failures never reverse local cancellation and are never returned through the cancellation acknowledgement.

This separation keeps runtime-work state truthful: `Cancelling` means the owning operation has not yet reported terminal completion, regardless of whether its best-effort cleanup signal succeeded.

### Live update ownership

Provider request fragments remain host-owned bounded transport facts. Complete request arguments
independently serve preparation, authorization, audit, and model execution. Gaps, lag, reconnect, or
truncation require a bounded replacement checkpoint; omitted bytes are not retained for
presentation.

Execution visuals are likewise replaceable current state. The SDK owns ergonomic replacement,
identity, sequence, limits, cadence, cancellation, and cleanup. The server owns validation,
accounting, attach/reconnect checkpoint hydration, closure, and durable latest-value checkpointing.
Plugins own schema interpretation and presentation. Generic host code must not add filesystem-, Vim-,
shell-, or provider-specific rendering branches.

```text
provider/plugin update -> bounded current invocation presentation -> SessionView item replacement
host terminal boundary -> flush latest update -> durable checkpoint + closed invocation
```

See [`plugin-live-progress.md`](plugin-live-progress.md) for producer examples, limits, privacy
requirements, and troubleshooting.

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

Persisted unplaced `ToolContribution` and legacy request-presentation fields are decode-only
compatibility facts. Session decoding retains their semantic payload, and projection assigns hidden
placement to unplaced contributions. TUI and web render paths do not inspect legacy presentation
metadata or infer visibility from old contribution schemas. New primary presentation writes use the invocation-scoped update contract. New
`ToolContributionEnvelope` writes are limited to migration compatibility and explicitly independent
supplemental output; the append-only `ToolContributionPlaced` event remains readable for historical
sessions.
