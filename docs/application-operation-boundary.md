# Application operation boundary

Bcode application behavior is reusable without making a second transport or a generic application framework. The current boundary is a set of focused, server-owned operation modules backed by portable domain contracts.

## Canonical call path

```text
CLI, TUI, HyperChad, SDK, or future adapter
→ typed `BcodeClient` method or shared renderer action
→ local IPC framing and exhaustive request routing
→ focused `bcode_server` operation
→ owning session, runtime, workflow, permission, plugin, or configuration domain
→ typed result or bounded ordered event stream
```

An in-process caller begins at the focused operation and uses the same domain owners. It does not construct IPC requests, open sockets, or read persistence directly.

## Operation responsibilities

Focused operation modules own reusable application coordination, including:

* Normalized authorization facts and authorization before side effects.
* Canonical domain invocation and typed results.
* Bounded reads, cancellation, ownership fencing, and terminal-state behavior.
* Explicit private context only where required, such as authenticated client identity or a connection-scoped event sink.

Operation modules do not own wire framing, request IDs, response encoding, handshakes, daemon artifact negotiation, sockets, connection lifetime, or frontend rendering. Workflow operations mechanically enforce this distinction through `scripts/check-workflow-architecture.sh`.

## Adapter responsibilities

The local IPC adapter owns:

* Exact daemon artifact startup and handshake behavior.
* Request and response framing, request IDs, and payload encoding.
* Exhaustive `RoutedRequest` classification.
* Connection attachment, lifecycle, and bounded event delivery.
* Mapping typed normalized failures to public transport errors.

A future concrete adapter must perform equivalent plumbing and call the existing typed operations. It must not copy application behavior, inspect canonical persistence, reinterpret event logs, or expose server-private types.

## Streaming semantics

Snapshots and event streams transfer current state and bounded live updates. They are not durable resume protocols. Any future adapter that claims resumability must separately define retention, acknowledgment, replay, gap, duplicate, conflict, and reconnect semantics.

Connection-scoped subscriptions receive an explicit event sink and return the durable sequence immediately after the initial snapshot boundary. Disconnect cleanup remains transport-owned.

## Plugin interactions

Plugin workflows remain plugin-owned. Application and renderer adapters may use plugin-contributed semantic controllers and typed `InteractionInput` when available. Schema-aware raw structured resolution remains the generic fallback. Unknown producer schemas or versions are surfaced rather than guessed.

## Verification

Representative coverage combines direct in-process operation tests with local IPC integration tests. Workflow source application currently exercises both paths and verifies identical created/updated outcomes and canonical store effects. Completion of the broader application-boundary effort still requires representative session, permission/interaction, runtime, workflow, and plugin coverage.
