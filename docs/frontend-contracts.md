# Frontend event and snapshot contracts

Bcode's SDK frontend contract is renderer-neutral and does not depend on TUI view models, daemon
IPC, a web framework, or plugin manifests. Terminal, desktop, web, and service applications can
serialize the same `FrontendEventEnvelope` and `FrontendSessionSnapshot` JSON values.

## Versioning and correlation

`FRONTEND_CONTRACT_SCHEMA_VERSION` versions both event envelopes and snapshots. Schema version 2
adds portable ordered assistant-segment and structured-reasoning-part stream operations. Every
envelope has a session ID, application/runtime turn ID, and monotonic per-turn delivery sequence.
`FrontendEventCursor` projects normalized runtime events and allocates delivery sequences; omitted
internal events consume no sequence.

Ordered stream payloads carry their own generation, revision span, accepted byte offset, checkpoint,
and terminal state. These integrity coordinates are independent from the envelope delivery sequence:
the envelope orders frontend delivery, while the stream update validates one semantic text stream.
`FrontendSessionSnapshot::apply_event` requires contiguous envelope delivery, accepts byte-equivalent
redelivery idempotently, rejects conflicting duplicate sequences, rejects mixed sessions/active
turns, and allows a new turn only after the previous turn is terminal and the next event is
`TurnStarted`. Snapshots retain payload fingerprints only to validate duplicate delivery; they do
not claim transport reconnection or durable event-log semantics.

## Provider/plugin isolation

The public frontend event enum contains normalized text, reasoning, tools, usage, exact input-token
counts, warnings, retries, errors, completion, and cancellation. Provider request projection and
opaque provider metadata are not representable and are omitted by `FrontendEvent::from_agent_event`.
Provider error codes are not exposed; only the already-normalized safe message enters the frontend
error event.

Transcript projection retains neutral text, image, tool-call, and tool-result blocks. Provider
extensions and cache points are omitted. No TUI or plugin types occur in the contract.

## Workflow authoring contracts

Workflow authoring uses versioned portable catalogs, documents, semantic edit batches, diagnostics,
compilation previews, drafts, conflicts, revisions, and start operations. Frontends and plugin-owned
surfaces consume those contracts through the application boundary; they do not access workflow-store
rows or daemon-private objects.

Manual editors mutate `WorkflowAuthoringDocument` through bounded semantic operations rather than a
renderer-private graph or unrestricted JSON Patch. Graph positions, groups, comments, and editor
hints remain namespaced presentation metadata excluded from executable identity. The TUI owns only
terminal layout, viewport, hit testing, input mapping, and drawing; web and desktop clients may use
native canvases without changing semantics.

Prompt-driven authoring consumes the same catalog and diagnostics, produces the same document, and
may save only an explicitly accepted optimistic draft. It cannot publish, activate, start, grant
permission, persist secrets, invent plugin contracts, access private application state, or resolve a
conflict automatically. Exact editor and generator boundaries are defined in
[`composable-coding-workflows.md`](composable-coding-workflows.md).

## Snapshot semantics

A session snapshot contains the visible projected transcript, optional materialized turn, and next
expected sequence. Turn state includes active/completed/cancelled/failed status, accumulated text
and reasoning, usage, exact request-input tokens, tools/results, warnings, normalized terminal error,
stop reason, and latency. `AgentSession::frontend_snapshot` creates a snapshot directly from visible
SDK session state.

These contracts are state-transfer primitives, not a resumable network protocol. Applications that
claim reconnect/resume must durably retain envelopes, preserve sequence/fingerprint history, and
define retention/acknowledgment behavior in their transport layer.
