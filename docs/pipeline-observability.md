# Pipeline diagnostic boundaries

Operational telemetry explains execution; it does not confer execution authority,
replace canonical history, or prove product quality. Use the existing metrics,
startup reports, runtime-work and session/workflow inspection contracts. Domain
owners instrument their boundaries; do not create a second execution registry or
infer provider-private behavior in the host.

## Required evidence

A diagnosable operation needs correlated entry, wait, completion and cleanup
observations. Distinguish admission, authorization, resource acquisition, execution,
receipt publication, durable settlement and runtime retirement. A terminal model
response does not imply that receipt publication or cleanup has settled.

* Use fixed operation and stage names. Correlation uses host-owned opaque session,
  turn, run, activation, attempt and dispatch identities, where available.
* Capture originating correlation before spawning or crossing task-local scopes.
  A cleanup task's ambient identity must not replace the originating operation.
* Measure elapsed intervals with a monotonic clock. Wall timestamps support joins,
  not elapsed-duration arithmetic across processes.
* Explicit completion records the known outcome. A metrics span dropped without
  completion records `unfinished`: this is neither success nor proof of cancellation.
  Distinguish a cancellation request, observed cancellation, shutdown notification,
  closed shutdown channel and ownership deferral at their owning boundary.
* Entry without exit can explain a pending boundary only within the observed scope.
  Missing, dropped, truncated or unavailable telemetry must remain explicit unknown.
* Queue wait and lock-held work are separate intervals. Identify a holder only when
  the owning synchronization contract supplies it; a file's PID text is not verified
  ownership and must not be presented as such.

## Coverage boundaries

| Boundary | Evidence needed | Evidence that is insufficient |
| --- | --- | --- |
| Daemon startup | Exact artifact/state scope; startup-lock wait; acquired or existing daemon ready; preparation, spawn, readiness, handshake; timeout/error | A stale lock-file PID or readiness of a different artifact/state |
| Workflow owner | Admission and authority check; shared-session permit; writer queue; receipt publication; observation callback; durable settlement; retirement | Final provider response or an empty live registry alone |
| Model/tool/permission | Originating work and attempt; request build, permission wait/decision, provider/tool start, completion, cancellation and cleanup | A polling heartbeat, tool label, or successful advisory call |
| Context/judgment | Selected contribution identity; actual consumer application; final request representation; provider conversion receipt; mandatory-context retention or explicit omission | Candidate selection, `applied=true`, estimated request size, or reconstructed request content |
| Evaluation | Exact attempt and canonical observation scope; capture completeness; independent outcome truth; disabled baseline | Compilation, mock success, complete capture or workflow success as proof of quality |

Existing timing or scalar observations are not necessarily sufficient to close these
boundaries. In particular, contribution delivery across provider conversion requires
an implemented normalized lineage contract; absence of that receipt remains unknown.
Historical lock ownership cannot be recovered from observations that were never retained.

## Privacy, bounds and access

Default diagnostic additions are content-free. Do not log prompts, tool arguments,
results, SQL, credentials, environment values, provider payloads, arbitrary errors or
filesystem paths as metric labels. Keep identity-bearing timeline observations
separate from deliberately low-cardinality product metrics. Reuse bounded retention,
nonblocking telemetry queues, dropped-event accounting and authorized paged reads.
Telemetry must not introduce execution locks or alter permission/dispatch outcomes.

Sensitive retained request/tool traces remain explicit opt-in diagnostic data. Do
not enable capture implicitly to improve scalar coverage or bypass the authorized
trace export API. See [session trace export](session-trace-export.md) and
[eval introspection](eval-introspection.md) for capture and completeness boundaries.

## Behavioral acceptance

Hold each real boundary deterministically, observe its correlated wait, release it
and observe completion. Exercise errors, caller abort, cancellation, owner deferral,
shutdown, detached cleanup and competing independent roots. Verify origin identity,
explicit outcome stability, bounded reads/retention, loss reporting, secret exclusion
and disabled-capture behavior. Include the concurrent failing workload: an isolated
passing probe does not establish diagnosis or closure of an aggregate failure.

For evidence delivery, verify the actual final request and provider adaptation, not
an estimate or an earlier selection. Missing, foreign, conflicting, unsupported and
incomplete receipts fail closed. Mechanics tests prove observability and delivery;
independent end-task comparisons remain necessary to establish quality benefit.
