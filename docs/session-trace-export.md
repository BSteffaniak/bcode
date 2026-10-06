# Session trace export

Trace export is explicit, sensitive diagnostic access to already retained content.
It neither enables capture nor reconstructs inputs from subsequent events. It is
independent of evals and works for ordinary and workflow execution sessions.

## Application contract

`ReadSessionTraceBlobRequest` and `SessionTraceBlobRange` live in session models.
The compatibility boundary is `SESSION_TRACE_EXPORT_VERSION = 1`. Unknown request
versions and fields are rejected; clients must reject unknown response versions.
No persisted session event or trace-reference encoding changes are required.

Request fields:

* `version: u16`
* `session_id: SessionId`
* `sequence: u64` — exact canonical referring event sequence
* `offset: u64` — byte offset in retained content
* `length: u32` — positive bounded requested count

Response fields:

* `version: u16`, `session_id: SessionId`, `sequence: u64`
* `sha256: String`, `content_type: String`, `byte_len: u64`
* `redaction: TraceRedaction`, `completeness: TraceBlobCompleteness`
* `offset: u64`, `bytes: Vec<u8>`, `eof: bool`

Both verification and returned ranges are capped at one MiB by
`MAX_SESSION_TRACE_BLOB_BYTES`. Each operation verifies the entire retained blob,
including bytes outside a requested range. There is no persistent verification
cache. Larger references return `trace_export_too_large` rather than accepting
unverified ranges or doing unbounded work. EOF returns an empty successful range;
an offset beyond retained EOF or an overflowing range is invalid.

The application operation is:

```rust
pub async fn read_range(
    state: &ServerState,
    request: ReadSessionTraceBlobRequest,
) -> Result<SessionTraceBlobRange, ReadTraceBlobError>
```

Public errors have stable codes and fixed, secret-safe messages:
`trace_export_unsupported`, `invalid_trace_range`, `trace_reference_not_found`,
`trace_unavailable`, `trace_content_missing`, `trace_content_damaged`, and
`trace_export_too_large`. Adapters must not replace these messages with internal
IO errors, paths, or trace contents.

## Authority and confinement

Authorize diagnostic access to the selected canonical session before calling the
operation. No caller-supplied path or blob reference is accepted. The operation
reads exactly one event using the bounded session event range API; it checks the
session ID, exact sequence and current event schema. Only `ModelRequestBuilt`,
`ToolInvocationStarted` and `ToolInvocationFinished` retained references are
supported. Missing references are not reconstructed or inferred.

References must use the writer's exact layout:
`<session-id>/blobs/<safe-name>-<lowercase-sha256>.json|txt`. The corresponding
media type must be `application/json` or `text/plain`. Traversal, extra components,
backslash spellings, alternate sessions and inconsistent hash names fail closed.
The trace-store root is internal trusted server configuration, never a public
operation argument. An empty root is unavailable, not the working directory.

The operation reuses the session artifact domain's bounded logical read and Unix
descriptor-relative no-follow opens. Untrusted session, blob-directory and file
components cannot be symlinks. Artifact domain storage representation rules remain
in force (including supported versioned compressed containers). Non-Unix platforms
fail unsupported rather than falling back to a race-prone ordinary file open.
Storage admission is held during the canonical read and moved into blocking IO so
cancellation cannot release it while content is still being read. Existing domain
access accounting retires successful admission; failed reads retire without
recording content consumption. No event history, projection, migration or repair
is written by trace export.

Retained logical length and complete retained SHA-256 must agree with the reference
before any payload bytes reach the caller. Successful verification does not upgrade
`Retained` or `Truncated` to `Complete`. `eof` only identifies the end of retained
bytes. An automatic-redaction marker is not a guarantee of secret-free user text
or tool output. The bytes remain sensitive; keep them out of routine frontend
snapshots, logs, permission descriptions and public error messages. This operation
does not return the private reference path or provider authentication/configuration.

## Integration

Minimal registrations in owning modules:

```rust
// packages/session/models/src/lib.rs
mod trace_export;
pub use trace_export::{
    MAX_SESSION_TRACE_BLOB_BYTES, ReadSessionTraceBlobRequest,
    SESSION_TRACE_EXPORT_VERSION, SessionTraceBlobRange,
};

// packages/server/src/lib.rs, next to artifact_operations
pub(crate) mod trace_operations;
```

Add a typed IPC request `ReadSessionTraceBlob { request:
ReadSessionTraceBlobRequest }` and a response carrying `SessionTraceBlobRange` to
the existing independently versioned transport boundary. Update compatibility
negotiation as required by that boundary before routing new variants. Dispatch
through `trace_operations::read_range`; forward failures with `error.code()` and
`error.to_string()`. This remains separate from ordinary artifact range access;
that operation does not confer trace-store authority. Mirror the typed operation
in application/client APIs, not a direct client filesystem accessor.

A CLI export command should explicitly select session, canonical sequence and
byte range, preserve binary bytes and completeness, and require intentional
sensitive capture/export use. It must not expose provider configs, silently enable
capture, imply redaction guarantees, or label partial/retained captures as exact
complete model inputs. Eval callers can use their existing canonical introspection
sequence to request the corresponding blob. Workflow callers use each execution
session's own identity and sequence, not a parent-session reference.

## Coverage and limits

Behavioral tests cover traversal and alternate paths, cross-session references,
canonical identity and schema, unsupported representations and request versions,
invalid ranges, oversized captures, partial ranges and EOF, completeness/redaction
preservation, corruption outside the requested range, missing and changed-length
content, and symlink escapes at each untrusted component.

Integration must still validate transport authorization/compatibility, response
routing and canonical degraded-storage behavior end to end. Captures above one
MiB are explicitly unavailable through this bounded verification contract; a future
larger-blob protocol needs domain-owned incremental verification, not a caller
filesystem escape or unbounded hash pass. Existing persisted completeness is
reported as recorded; historical capture precision is not repaired by this read.
