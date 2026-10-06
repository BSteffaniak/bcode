# Representation-aware workflow observation

Public aggregate inspection and graph pages route by verified canonical run
representation. Legacy reads remain unchanged. New-family inspection reports
canonical status, registered definition checksum, revision-qualified graph prefixes,
activation metadata, attempts, verified outputs, direct child links and verified
execution-session provenance. Terminal output is read only for completed runs:
an intermediate exit output must not be mistaken for authoritative completion.

`unavailable_collections` is an additive inspection field. Older senders omit it;
new-family readers list unimplemented evidence explicitly. Empty values for those
collections do not establish absence. Eval capture marks these observations
incomplete. New-format semantic event history is rejected rather than invented
from attempts. Attempt pagination uses indexed dispatch identity, verifying the
cursor's prepared timestamp; legacy timestamp ordering remains unchanged.

Run listing uses mixed identity discovery and verifies each returned summary.
New-family frontend nodes report persisted completion, skipping and failed/cancelled
attempt outcomes; unresolved attempts are not guessed to be running.

Retry, pause/resume, cancellation, detachment and allowance changes already have
representation-aware adapters. Replacement admission is now explicitly guarded
before legacy preparation. Explicit new-family attempt repair remains unsupported
and returns capability-unavailable rather than searching only legacy history or
repeating ambiguous side effects. Full replacement/repair implementation and a
new-format semantic event ledger remain outstanding; these guards are not closure
of those capabilities.

Observation errors after eval admission request cancellation and retain the
request result. Failed verification never establishes that effects were absent.
Further terminal verification and infrastructure-versus-quality reporting remain
necessary for fully calibrated comparison runs.
