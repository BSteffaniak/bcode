# HTTP finalized-payload lease (v1 convergence)

Source contract; not yet compiled or tested. This is transient diagnostic ownership,
not durable replay, canonical storage, task completion, or persistence completion.

* `POST /v1/xray/observe`: unchanged. Active idle subscriptions have no lifetime
  deadline by default, remain hot across turns, and emit heartbeat comments.
* `POST /v1/xray/finalize` with
  `{"schema_version":1,"observation_id":"<id>"}`: unchanged request. A 200 JSON
  `XrayFinalization` is a cutoff receipt only. It disables this observer's capture,
  does not release payload ownership, and does not certify remote stream drain.
* `POST /v1/xray/payload`: unchanged bounded range schema and authorization.
  Fetch references both while streaming and after `xray.finalized` / SSE EOF.
  Successful reads renew the finalized-payload inactivity lease. Honor the range
  limit advertised by the ready envelope; missing/evicted payloads remain explicit
  failures, never reconstructed or treated as complete captures.
* **New** `POST /v1/xray/release` with exactly
  `{"schema_version":1,"observation_id":"<id>"}`. Unknown fields/versions fail.
  Uses the same bearer, endpoint-local observation identity, and session grant.
  Before successful cutoff returns 409 `{"code":"xray_finalize_required"}`.
  After cutoff requests destruction of observer/payload ownership and returns 204
  only after that ownership retires. Concurrent release requests coalesce. Unknown,
  expired, or already retired identities return 404
  `{"code":"xray_observation_unavailable"}`; no replay tombstones are retained.
  A 503 `xray_release_pending` means retirement is not yet confirmed, not rollback.

Consumer sequence: attach and consume ready; capture live events; request finalize;
consume through finalized; fetch/export all needed retained references; release;
close the bootstrap endpoint when done. `finalized` proves only the engine cutoff.
Receiving it never establishes that referenced payloads were downloaded. Partial,
missing or failed payload exports must keep recording evidence incomplete.

Finalized observers are retained independently of SSE lifetime. Their lease expires
60 seconds after finalization delivery processing or last successful payload read,
whichever is later. Explicit release, endpoint close, or server shutdown also ends
it. A stalled reader is still subject to bounded send timeout. Finalized retention
counts against the existing observation, queue and payload limits; `active_count()`
includes it so bootstrap must not reclaim an endpoint as unused during exports.
There is no event reconnect/replay or lifetime expiry for active idle subscriptions.

Client integration required: add `release(observation_id)` with an operation deadline;
call after export/drain, not from finalize. Treat 404 after a previously known cutoff
as already unavailable for cleanup only, never as proof of successful export.
Existing clients can still finalize/drain but will leave a bounded 60-second lease.
Ready-envelope capabilities remain the capability discovery boundary.
