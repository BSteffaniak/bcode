# Frontend event and snapshot contracts

Bcode's SDK frontend contract is renderer-neutral and does not depend on TUI view models, daemon
IPC, a web framework, or plugin manifests. Terminal, desktop, web, and service applications can
serialize the same `FrontendEventEnvelope` and `FrontendSessionSnapshot` JSON values.

## Versioning and correlation

`FRONTEND_CONTRACT_SCHEMA_VERSION` versions both event envelopes and snapshots. Every envelope has a
session ID, application/runtime turn ID, and monotonic per-turn sequence. `FrontendEventCursor`
projects normalized runtime events and allocates sequences; omitted internal events consume no
sequence.

`FrontendSessionSnapshot::apply_event` requires contiguous delivery, accepts byte-equivalent
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

## Snapshot semantics

A session snapshot contains the visible projected transcript, optional materialized turn, and next
expected sequence. Turn state includes active/completed/cancelled/failed status, accumulated text
and reasoning, usage, exact request-input tokens, tools/results, warnings, normalized terminal error,
stop reason, and latency. `AgentSession::frontend_snapshot` creates a snapshot directly from visible
SDK session state.

These contracts are state-transfer primitives, not a resumable network protocol. Applications that
claim reconnect/resume must durably retain envelopes, preserve sequence/fingerprint history, and
define retention/acknowledgment behavior in their transport layer.
