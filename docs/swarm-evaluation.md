# Swarm evaluation

The eval package remains a leaf. `executor = "workflow"` exercises an explicitly
identified executable through public CLI boundaries, never canonical databases.
`metadata.workflow` contains the strategy, authored stop condition, allowances,
optional judge selection, expected report substrings and preservation paths.

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

## Scope still requiring expansion

This initial implementation is not the complete proposed research harness. The
substring oracle is appropriate only for tiny exact-report fixtures; it does not
prove arbitrary correctness and can match intermediate reports. Full structural
answer/hidden-test grading, four-way direct/goal entry comparisons, interleaved
randomized paired scheduling, confidence intervals, equal-resource policies,
parallel speed fixtures, held-out coding scenarios, scripted swarm/judge faults,
complete large-graph output/descendant pagination, cancellation terminal verification
and automatic failure diagnosis remain to be implemented. Capture limits are not
proof of complete composed-run history. Worker-ID repetition is an observation,
not itself proof that repeated work was unnecessary. Command deadlines terminate
the direct child; generic detached descendants require stronger process-group handling.
Do not promote the live suite to a calibrated release quality claim yet.
