# Representative swarm task families v1

`../representative-v1-train.toml` and `../representative-v1-holdout.toml`
are **candidate, unexecuted** suites. Each has four cases, four strategies
(direct/single/auto/collaboration), and three repetitions: 48 scheduled attempts.
They retain the legacy seeded scheduler (seed `20261012`), not a newly balanced
policy. No superiority, quality noninferiority, or cost-saving claim follows.
The frozen controlled/introspection suites and grading are unchanged.

## Tasks and split

| Training/development family | Held-out family | Purpose |
| --- | --- | --- |
| posted-amount parsing/validation/application | offset-aware half-open timestamp windows | small, multi-file debugging; staying local is reasonable |
| ledger/inventory/path/word aggregation | intervals/routes/median/tag intersection | four independent modules plus integration, eligible for useful parallel contributions |
| normalization → ranking/checksum | exact cents → transient publisher → bounded retry | typed dependency, intermediate consumption, sequential failure recovery |
| cache-miss and billing review with safe distractor | catalog default and negative-transfer review with safe distractor | defect precision/recall without fixing source |

Train is the development split. Holdout uses distinct problem families rather
than new numeric seeds of training tasks. Do not tune against holdout outcomes,
remove losing cases, change prompts after results, select repetitions, or replace
failed configuration attempts. These are eight fixed families, not enough for
strong independent-task generalization. The holdout assets are repository-visible;
this is an experimental split, **not confidential benchmark protection**.

## Grading and controls

Only `workspaces/<case>/` is copied into a subject workspace. `oracles/`,
`controls/`, and `tests/` remain outside it; expected answers, reference patches
and hidden inputs are not supplied in prompts or fixture files. Repository-wide
access could still expose them and must be recorded, not described as secrecy.

The external parent grades `answer.json`, byte-exact input/TASK preservation,
and three functional probes per coding case. Handoff additionally rejects
repeated identifiers; recovery additionally checks retry exhaustion and invalid
sub-cent amounts. Fresh subprocesses return versioned value/exception observations;
only the parent compares these against independent expected values. Duplicate JSON
keys, float/bool substitutions, missing/extra fields, mutation, wrong values,
symlinks, oversized responses, early exit, crashes and loops fail closed.
All six coding references and both review reference reports are independent
controls; the producer does not import the oracle. Original buggy subjects reject
even with a correct visible artifact. Review initial controls have empty findings.

Review grading is a deliberately bounded structured rubric: exact defect
file/function coverage, medium severity, concrete explanation terms, and no
false-positive findings. It is **not semantic equivalence grading of arbitrary
prose**. Category strings are descriptive, not a complete taxonomy. Review
sources are preserved byte-for-byte.

Run offline controls, without providers or daemons:

```sh
python3 -B -W error::ResourceWarning -m unittest discover \
  -s fixtures/evals/swarm/representative-v1/tests -p 'test_*.py' -v
```

For a temporary reference copy only:

```sh
python3 fixtures/evals/swarm/representative-v1/controls/reference.py \
  train-local /absolute/temporary/workspace
BCODE_EVAL_WORKSPACE=/absolute/temporary/workspace \
  python3 fixtures/evals/swarm/representative-v1/oracles/grade.py train-local
```

## Boundaries and execution prerequisites

* Process separation is not a Python sandbox. Same-user malicious code can forge
  valid responses, inspect the interpreter, mutate external files, or signal other
  processes. The protocol does not attest function execution. Use an actual OS
  isolation boundary for adversarial security claims.
* Correct files/functions do not prove workflow completion, worker usefulness,
  predecessor consumption, daemon cancellation, or exactly-once external effects.
  Those are separate runtime facts; legacy combined success remains unchanged.
* Recovery injection is deterministic **subject algorithm** behavior (first N
  attempts fail). It proves only the fixture's retry/no-failed-commit contract.
  The runtime control lane must separately test cancellation during provider/tool
  work, stale completion after cancellation, durable authority fencing, and
  ambiguous-side-effect retries; oracle output cannot certify these conditions.
* Manifests use an explicitly identified existing canonical runtime executable.
  They do not enable `adaptive-v1`: adapter support must be verified and a separately
  identified candidate manifest created before that experiment. No guessed flags.
* No model, profile, provider or pricing substitutions are declared. A future run
  must preserve the baseline launch selection and record resolved provenance,
  permissions and executable identity. Current selection alone does not reproduce
  historical inline configuration. Unknown cost stays unknown.
* These manifests have Python TOML/asset checks, not yet Rust parser admission or
  live execution validation. Coordinator must validate them through the typed eval
  parser when its compiler resource is available. Launch authorization is separate.
