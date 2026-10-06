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

Execution-session discovery follows the indexed canonical attempt identities and
looks up exact session links. The link table contains only dispatch identity and
payload, not a run-ID column. Filtering it by run ID previously hid child sessions
and their telemetry. The bounded attempt prefix is now used explicitly; saturated
coverage is reported as incomplete rather than treating a short session list as
exhaustive. Live trials now retain fresh evaluator, coordinator and worker transcripts
and account for their usage.

If the agent evaluator accepts completion but judgement withholds it, correction
feedback targets the evidence disagreement and reuse of the existing artifact,
not blind reimplementation. The judge question excludes invented deliverables and
circular rejection because judgement itself is pending, without weakening the
original condition or threshold. This is not proof of provider calibration.

The loop judgement request includes the evaluator's summary alongside its evidence
and pinned stop condition. The summary is an untrusted evaluation conclusion, not
completion authority; the agent completion gate and configured probability threshold
still both apply. Judge output appends the probability decision instead of discarding
the evaluator summary, retaining corrective context and the delivered report.
Request bounds include the added summary. Fake-provider tests verify the handoff,
threshold rejection, and that even 100% judgement cannot promote a negative agent
assessment.

Repeated live persisted-report trials still show oracle-correct artifacts rejected
by judgement, including after the fresh evaluator accepts preservation. Summary
handoff repairs missing context but does not establish judge calibration or eliminate
rejections. Eval diagnosis explicitly reports independent-oracle/completion disagreement;
it does not label it a verified provider root cause.

A controlled rerun accepted artifact correctness in all six trials and fresh
evaluators accepted preservation, but several completion judgements remained below
the unchanged 90% threshold. The evidence gap fix therefore does not establish
closure of judgement calibration, complete execution telemetry, or general swarm
performance. Two repetitions on one task cannot support a general efficiency claim.
Further judgement evidence experiments must preserve the acceptance threshold and
include corrupted artifacts and misleading claims as negative controls.
