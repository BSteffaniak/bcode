# Plugin Live Progress

Plugins should use live progress for replaceable presentation state that is useful while an invocation is active but is not part of durable session history. Examples include a Vim frame, terminal recording checkpoint, or another bounded execution preview.

## Execution progress

Use `bcode_plugin_sdk::TransientProgressPublisher` rather than constructing transient contribution envelopes manually. The publisher owns:

* invocation and contribution identity
* producer/schema identity
* `Transient` persistence and `Progress` placement
* monotonic sequence numbers
* host-advertised encoded-byte and cadence limits
* cancellation checks and idempotent terminal removal

Create the publisher from the invocation's `NativeServiceContext` values:

```rust,ignore
let mut progress = bcode_plugin_sdk::TransientProgressPublisher::with_limits_and_cancellation(
    context.events,
    invocation.tool_call_id.clone(),
    "screen",
    "example.plugin",
    "example.live-screen",
    1,
    context.transient_progress_limits,
    context.cancellation.clone(),
);

if progress.is_ready() {
    let frame = build_bounded_frame();
    let _ = progress.upsert(&frame)?;
}

// Call on success, failure, timeout, and cancellation paths.
progress.finish()?;
```

Prefer `upsert_if_ready` when payload creation is cheap. Check `is_ready` before expensive frame capture or serialization so superseded work is skipped before crossing the plugin ABI. Use `append` only when the schema defines bounded append semantics; independently renderable frames should use replacement.

`finish` is idempotent. Plugins must call it explicitly at every terminal boundary. The host also clears every active key for an invocation when the invocation terminates, covering plugin failure or process loss where plugin cleanup cannot run.

## Provider request drafts

Provider request drafts are different from execution progress. They contain incomplete argument bytes observed before a canonical request exists. The host transports them as bounded `ToolRequestDraftEvent` append batches and checkpoints keyed by turn and tool call. They are never executable and never enter authorization.

A plugin that advertises request-draft presentation owns best-effort interpretation of its draft schema. Treat malformed JSON, split escapes, truncated prefixes, and incomplete UTF-8-level JSON structure as normal assembling states. Presentation receives a bounded preview plus:

* tool and producer identity
* schema and schema version
* generation and revision
* retained preview start offset
* total observed argument bytes
* truncation state

Do not require successful deserialization before displaying a draft. Render known fields when available and otherwise use a compact assembling fallback. Never write partial filesystem content or start tool execution from draft bytes.

## Limits and truncation

Current host defaults are:

* transient contribution envelope: 256 KiB encoded
* active transient contributions: 256 keys per session
* aggregate active contribution state: 8 MiB per session
* request-draft retained preview: 128 KiB per tool call
* active request drafts: 256 keys and 4 MiB retained preview bytes per session
* default producer cadence: 50 ms between replaceable updates

The host remains authoritative even when a producer uses lower limits. Oversized updates are rejected rather than partially published. Request-draft producers switch to bounded checkpoints after truncation so byte offsets remain trustworthy. Omitted bytes are never retained merely to support presentation.

## Persistence, privacy, and telemetry

Live progress and request drafts use `SessionLiveEvent`, not `SessionEvent`. They are absent after daemon restart and must not be written to session databases, event logs, projections, catalogs, manifests, artifact finalization, trace blobs, workflow state, crash reports, or structured telemetry fields.

Metrics and logs may contain bounded classifications, counts, byte sizes, and rejection reasons. They must not contain progress payloads, draft bytes, unique paths, invocation IDs as metric labels, or secrets copied from request arguments. A canonical completed request follows the normal permission, audit, redaction, and persistence policy independently of its live preview.

## Troubleshooting

* **No updates appear:** verify the host supplied an event callback, the publisher schema matches the plugin manifest, and the invocation has not been cancelled or finished.
* **Updates are skipped:** producer cadence intentionally drops superseded frames. Force only the final bounded checkpoint before cleanup when visual continuity requires it.
* **An update is oversized:** reduce detail before serialization or emit a schema-defined bounded checkpoint. Do not split replacement frames into unbounded history.
* **A reconnect misses deltas:** this is expected. The client must consume the host's current bounded checkpoint/resynchronization state.
* **Progress remains visible:** ensure every terminal path calls `finish`; host invocation teardown is the final safety net.
