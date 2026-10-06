# Swarm evaluation

The eval package remains a leaf. `executor = "workflow"` exercises an explicitly
identified executable through public CLI boundaries, never canonical databases.
`metadata.workflow` contains the strategy, authored stop condition, allowances,
optional judge selection, expected report substrings and preservation paths.
Explicit variant models are selected through the public session API. External
workflow trials reject unapplied `profile` metadata; use an explicit configuration
overlay rather than silently claiming a model profile was exercised.

`bcode goal --stop-condition` bypasses generation so controlled variants use the
same authored objective and acceptance condition. The normal generated entry path
is unchanged. Authored input currently excludes progress-document generation.

## Running

```sh
BCODE_SWARM_EVAL_BINARY=/absolute/path/to/bcode bash scripts/check-swarm-eval.sh
BCODE_CONFIG=/path/to/providers.toml /absolute/path/to/bcode eval run \
  fixtures/evals/swarm/live.toml --output-root /tmp/swarm-evals \
  --run-id candidate --fail-under-pass-rate 1
```

The offline script scrubs credentials/configuration and retains artifacts. The
live suite uses explicit test-scoped workflow publication authorization and
isolated daemon state. Credentials remain selected through ordinary configuration.
Edit the manifest's binary identity for other machines. A binary SHA-256 is retained
in every workflow trial's `subject.json`.

## Results and feedback

Each trial retains admission, workflow inspection, canonical outputs, source and
bounded child-session transcripts, independent fixture preservation checks,
completion outcomes and measurements. `oracle.json` separates artifact matching,
preservation, false completion and completion failure. Jev is part of the subject,
not the independent oracle. Numeric usage is aggregated from observed transcripts;
pricing is not guessed. Worker activation repetition and judge calls are reported.
`paired.json` compares each case/repetition against variant `single`. A baseline
pass/candidate failure remains a regression even if another candidate trial improves.
Missing candidate observations fail paired quality. Speed is eligible only for pairs
where both pass. `metadata.require_paired_quality = true` makes that gate required.
The CLI's `--fail-under-pass-rate` also rejects failed required comparison gates.

## Observed baseline, 2026-10-04

Artifacts: `/tmp/bcode-swarm-live-evals/baseline-authorized`.
Two trials per strategy, exact same authored acceptance condition, Jev `jev-1.13.0`
with threshold 90%, two iterations, concurrency 2. Results are descriptive, not
statistical calibration or proof of general superiority:

| Strategy | Verified completion | Mean total trial time |
| --- | --- | --- |
| single | 2/2 | 64.7 s |
| auto | 0/2 | 120.3 s |
| collaborate | 1/2 | 401.7 s |

Three paired quality regressions. No false completions detected by the fixture
oracle. Auto judgement sequences were 0.78/0.84 and 0.28/0.26. Collaborate sequences
were 0.80/0.77 and 0.18/0.94. Single sequences were 0.92 and 0.86/0.95. Inspect
canonical evaluation outputs and transcripts for causes; do not label probabilities
accuracy scores. Offline product smoke passed after explicitly disabling unsupported
fake-provider prompt caching. A prior live admission failure identified missing
publication authorization in isolated config; execution failures now persist as
trial failures instead of aborting the report.

## Expanded measurement and grading

`metadata.paired_schedule_seed` enables deterministic interleaving by case and
repetition, with variant order shuffled inside each block. `schedule.json` records
the exact order. Pair reports now include Wilson 95% intervals and nearest-rank
p50/p95 latency ratios for successful pairs. These descriptive intervals do not
establish noninferiority or independent-task generalization.

Workflow oracles support `answer_file`, `expected_json`, and `oracle_command` in
variant configuration or case `metadata.workflow_oracle`. Exact JSON comparison
rejects extra/missing/wrong fields. Hidden validation commands run only after the
subject, with a separate deadline; keep their sources outside the workspace.
Report-only grading uses only terminal summary/evidence, excluding copied task
and stop-condition fields so echoing acceptance text cannot satisfy the oracle.
It never concatenates
intermediate summaries. Failed runs cannot pass from an intermediate correct quote.
`generated_entry = true` exercises ordinary goal generation rather than authored
controlled input. `diagnosis.json` records deterministic failure labels without
claiming a verified root cause.

On timeout, cancellation is requested for the exact admitted run and terminal
state is polled; failure to verify is preserved. Unix subprocesses run in their
own process group, which is killed on timeout. This does not fence detached
processes that deliberately create another group.

Validated interleaving fixture: `fixtures/evals/swarm/scheduling.toml` (six
command trials passed). `scripts/check-swarm-eval.sh` also runs a scripted false
completion control: the fake evaluator claims completion without delivering its
required JSON artifact, and the independent oracle must reject the trial with
`workflow.false_completion = 1`. A failed CLI invocation alone is not accepted as
proof; the script verifies retained result measurements.

Exact-JSON oracle and deadline behavior have deterministic
behavioral tests. Offline real-product workflow smoke remains passing.

`fixtures/evals/swarm/coding.toml` adds direct-agent, controlled no-delegation,
auto, collaborate, and generated-entry trials against hidden Python functional
checks outside the workspace. `oracle-calibration.toml` and the offline script
verify that corrected code passes and the intentionally broken fixture fails.
All variants receive the same task; grading checks
empty inputs, duplicate handling and complete summation. Command judges accept an
explicit `timeout_ms` and `{{suite_dir}}` locator. These task-level checks are
independent of the goal completion gate. The fixture is not yet a held-out benchmark.

Final graph pages are captured at one pinned revision through the public CLI.
Workflow event and attempt histories are paginated through their existing public
keyset APIs and retained as separate artifact pages.
Aggregate collection saturation marks capture incomplete rather than claiming
complete descendant/activation/output coverage.

## Evidence validity

Paired report version 2 separates execution failures from observed quality failures.
Missing trials and executor errors are unevaluable, not quality regressions or
successes; they block the comparison gate and are excluded from quality confidence
interval denominators. Incorrect independently graded artifacts still count as
quality failures when execution was observed. Incomplete capture blocks efficiency
eligibility and the comparison gate, even when the delivered artifact passes.

History collections are captured independently: unavailable semantic events no
longer prevent retention of canonical attempt pages. Capture failures remain
explicit and never establish absence of effects. Diagnosis version 2 includes
normalized attempt identities, statuses and timestamps with artifact locators;
it still does not claim verified root causes. Persisted receipt admission time
is retained rather than represented as missing.

## Scope still requiring expansion

This is not yet the complete proposed research harness. Equal-resource policies,
parallel speed fixtures, held-out coding scenarios, scripted swarm/judge faults and
complete output/descendant pagination remain. Current public aggregate inspection
and output CLI boundaries do not expose cursors for those collections; extending
them requires typed application/IPC support, not private database access. Capture limits are not proof of complete composed-run history.
Worker-ID repetition is an observation, not proof of unnecessary work. Failure
labels do not establish root cause. Do not promote the live suite to a calibrated
release quality claim yet.
