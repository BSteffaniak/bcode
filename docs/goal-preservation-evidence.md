# Preserved-artifact completion evidence

Controlled persisted-report trials grade report.json and fixture byte preservation
independently of workflow terminal status. The oracle checks exact JSON equality
and compares the fixture to the retained source fixture outside the subject workspace.
This distinguishes correct work that fails acceptance from incorrect work.

Fresh auto/collaboration evaluator contexts do not inherit implementation tool
transcripts. Summary-only claims of shell validation did not establish preservation
in the observed trials, even though the independent oracle accepted the files.
Shared-context single-agent evaluation had access to its validation observations.
This is an evidence-handoff failure, not evidence of incorrect artifact production.

Filesystem text reads now expose the SHA-256 of the full bytes already read and their
byte length, including when displayed lines are truncated. The digest observes one
read, not an atomic historical snapshot or proof that a file was never modified.
Implementation guidance requires pre-change baseline provenance and post-change
observations; read-only evaluators can independently compare the current digest.
Reported baselines remain contributor-supplied, not host-certified history.

A controlled rerun accepted artifact correctness in all six trials and fresh
evaluators accepted preservation, but several completion judgements remained below
the unchanged 90% threshold. The evidence gap fix therefore does not establish
closure of judgement calibration, complete execution telemetry, or general swarm
performance. Two repetitions on one task cannot support a general efficiency claim.
Further judgement evidence experiments must preserve the acceptance threshold and
include corrupted artifacts and misleading claims as negative controls.
