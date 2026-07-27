# Plugin Presentation Updates

Plugins use presentation updates for bounded replaceable visual state owned by a tool invocation. A
presentation does not have separate live and finalized objects: each accepted update replaces the
current value, and invocation closure preserves the last accepted retained value.

## Primary update contract

Each invocation owns one stable primary transcript item. The host supplies an invocation-scoped
presentation handle so plugins do not construct transcript IDs or coordinate request, progress,
result, promotion, and removal objects.

Conceptually:

```rust
let presentation = context.tool_presentation();
presentation.replace(&payload)?;
presentation.replace_artifact(&checkpoint)?;
```

The host binds invocation and producer identity, validates schema/version and payload bounds, assigns
or validates monotonic revisions, coalesces superseded values, and rejects updates after closure.
Plugins own opaque payload schemas and surface-specific adapters. Generic runtime and renderers do
not inspect payloads for lifecycle, timing, retention, path, shell, or edit semantics.

Existing `TransientProgressPublisher`, request-draft placement declarations, and placed contribution
envelopes are migration interfaces. They must converge through the same authoritative current-item
projection and are not the target API for new primary presentation behavior.

## Retention

Retention is independent from whether an update happened while execution was active.

### Retain latest

Keep one bounded current value while the invocation is open and checkpoint the latest accepted value
at terminal closure. Do not persist intermediate frames.

Use this for primary output expected to remain in history, including:

* shell terminal/recording checkpoints;
* filesystem source or diff presentation;
* Vim playback/diff presentation;
* other bounded or artifact-backed tool output.

### Active only

Keep the current value only while the invocation is open and remove it at closure. It must not enter
session databases, event logs, projections, catalogs, manifests, artifact finalization, trace blobs,
workflow state, crash reports, or structured telemetry.

Use this narrowly for:

* sensitive request fragments;
* spinners;
* intentionally ephemeral diagnostics.

### Supplemental

Independently meaningful supporting output may use an explicitly keyed supplemental item. A
supplemental item is not a phase-specific duplicate of the primary item.

## Ordering and closure

Updates and terminal closure share one per-invocation ordering boundary:

1. Stop accepting new producer work.
2. Flush already accepted updates.
3. Apply the latest accepted value.
4. Record terminal outcome, fixed timing, canonical model result, and retained presentation
   checkpoint.
5. Close the update scope.
6. Reject every later update.

Closed state is absorbing. A duplicate, stale, delayed, or even higher-revision live delivery cannot
reopen an invocation, restart elapsed timing, or replace terminal presentation.

Plugins normally do not manually finalize presentation. Returning from invocation or host teardown
closes the scope. A manual flush may be exposed for tools that must guarantee a final artifact
checkpoint before return, but it does not create a separate final presentation mode.

## Streamed request presentation

Provider argument fragments are host-owned observational transport data. A plugin may declare an
adapter schema for displaying the current bounded draft, but draft bytes never enter preparation,
authorization, or invocation. Complete validated arguments follow their normal permission, audit,
redaction, and persistence policies independently.

Contiguous updates use UTF-8 byte offsets. A gap, lag, reconnect, or truncation requires a bounded
replacement checkpoint. After truncation, producers continue with checkpoints so offsets remain
trustworthy. Omitted bytes are not retained merely for presentation.

The declared draft schema always updates the invocation's current primary presentation. Placement
is intentionally not configurable: request/progress/result slot selection was removed with the
legacy primary-slot architecture.

## Artifact-backed updates

Large or incrementally produced output should replace a bounded artifact reference/revision rather
than copying an unbounded payload on every frame. A checkpoint declares committed bytes, revision,
finalization, availability, and content type. Renderers fetch only required bounded ranges for
resident content.

Artifact references do not own invocation lifecycle. A finalized artifact may still arrive before
host closure, and an active artifact cannot keep a terminal invocation's elapsed timer running.

## Limits and cadence

The host enforces bounded encoded payload size, artifact range limits, and update cadence. Producers
should emit updates only when visible state changes. Superseded updates may be dropped, but the latest
accepted current value must remain available for attach/resynchronization.

Plugins must not bypass publisher limits through raw event callbacks. Repeated payload rejection or
serialization failure should be surfaced as bounded diagnostics without payload contents.

## Attach, reconnect, and replay

For an open invocation, attach/reconnect supplies the current bounded checkpoint plus its generation
and revision. A client that misses updates replaces local state from that authoritative checkpoint.
It does not replay every intermediate frame.

For a closed invocation, durable replay reconstructs the latest retained checkpoint. The terminal
live document, immediate reconnect document, and replayed document must converge to the same semantic
item.

## Persistence, privacy, and telemetry

Metrics and logs may include bounded classifications, counts, byte sizes, cadence, and rejection
reasons. They must not include presentation payloads, request fragments, unique paths, invocation IDs
as metric labels, or secrets copied from tool arguments.

Raw plugin payload JSON is never a normal transcript fallback. Unsupported adapters render bounded
host metadata or outcome fallback.

## Compatibility

Historical `ToolContributionEnvelope` placement remains decodable for the supported history contract.
It is not scheduled for time-based removal: removal requires an explicit supported-history migration
or version cutoff, fixtures proving older sessions are intentionally no longer supported, and
coordinated updates to persisted decoding, compatibility projection, architecture documentation,
and mechanical guards. Chronological compatibility projection maps the latest compatible
request/progress/result contribution to the invocation's current primary item, preserves explicit
supplementals independently, and keeps hidden/unplaced payloads invisible. New primary producers
must not rely on this placement model.

## Troubleshooting

* **No updates appear:** verify the host supplied a presentation handle, the schema/version matches a
  manifest adapter, payload limits are satisfied, and the invocation is still open.
* **Updates are skipped:** cadence intentionally drops superseded values. Emit a bounded checkpoint
  when continuity requires it.
* **An update is oversized:** reduce detail or publish an artifact-backed checkpoint. Do not split
  replacement state into unbounded history.
* **Reconnect shows stale state:** verify generation/revision hydration and force authoritative
  snapshot replacement after a mismatch.
* **Progress remains visible after completion:** verify host closure ran. Do not add renderer cleanup
  special cases; closed-scope projection must reconcile the authoritative item.
* **Elapsed time continues after completion:** verify invocation lifecycle is terminal. Presentation
  payloads and generic transcript streaming flags must not schedule tool timers.
