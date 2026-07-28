# Workflow operator status, doctor, and repair

## Bounded status and history

Normal workflow list, status, inspect, and history operations are read-only and bounded. They query normalized projections and never replay the complete workflow event log, contact external owners, or perform repair.

The workflow status surface exposes:

* run and exact definition identity;
* activation node, generation, durable status, and next eligible action;
* waiting input/approval records and exact pending mutation approvals;
* attempts, dispatch identities, receipt presence, and terminal timestamps;
* branch/repeat/parallel decisions;
* grants and resource leases;
* validated output schema IDs and artifact references;
* bounded events and child sessions.

Use paged event and attempt APIs for additional history. Artifact references are opaque owner-produced references; status does not read entire artifacts or command output.

## Diagnosing a run

Run explicit doctor for a suspected damaged or ambiguous run. Doctor is bounded and non-mutating. It reports:

* disagreement between run status and repair-required attempts;
* activation/output status mismatches;
* persisted dispatch identities inconsistent with run/node/activation/attempt components;
* truncation when the requested inspection bound prevents a complete report.

Missing or incompatible plugin blocks and template requirements are diagnosed at registration, template catalog/describe, and start. A disabled owner is surfaced rather than hidden or substituted.

Pending mutation grants are checked against exact scope immediately before dispatch. Stale, expired, wrong-workspace, changed-input, wrong-node/activation, or incompatible block grants fail closed and never dispatch.

## Orphaned and ambiguous attempts

Prepared/admitted/running attempts are identified by stable dispatch identity. Cancellation and restart reconciliation use persisted intent and optional owner receipt.

A read-only attempt without a receipt may be eligible for the owner's explicit replay policy. A mutating prepared attempt, or an accepted mutating attempt whose terminal outcome cannot be proven, becomes `repair_required`. Generic runtime never guesses success and never automatically replays it.

For shell command plans, inspect persisted normalized command-plan identity, receipt, process/owner evidence, and typed artifacts. If no trustworthy receipt or owner status proves an outcome, preserve repair-required state. Arbitrary commands are not replayed merely because the process disappeared.

For Git commits, compare persisted expected HEAD and paths with `git.commit-status` evidence:

* `not_committed` supports explicit failure/cancellation resolution;
* `candidate_commit` supports explicit success only after exact commit/path evidence is verified;
* `diverged` remains ambiguous and requires operator investigation.

Always correlate owner evidence with the exact dispatch identity. Do not treat a later unrelated commit as proof.

## Explicit repair

Repair accepts one exact repair-required dispatch identity and one typed resolution:

* confirm success with a schema-valid `ValidatedOutput` matching run/node/activation;
* confirm terminal failure;
* confirm cancellation;
* abandon for a later explicit retry.

Repair records an event and updates durable attempt/run state. Abandonment does not dispatch and does not lower permission requirements. A later retry uses a higher attempt number and a new dispatch identity.

Completed, cancelled, failed, and repair-required terminal transitions remain authoritative. Resume/retry operations that are not valid for the current state fail with structured lifecycle errors.

## Migration and backup safety

Normal status/open/history paths never migrate, rebuild, or repair. Workflow-store schema migrations run only through the defined migration ledger and fail closed on unsupported future state.

Before any destructive rebuild, reindex, or migration of user-created workflow state:

1. stop all writers and acquire exclusive maintenance ownership;
2. create and verify a backup of the canonical workflow database and owned artifact roots;
3. record source schema/version and checksums;
4. run the explicit maintenance operation;
5. validate canonical definitions, runs, attempts, outputs, and receipts before reopening writers.

Derived indexes and projections are disposable; they never replace the canonical workflow database or external owner evidence.
