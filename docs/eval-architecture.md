# Eval Architecture

Bcode evals are a leaf capability for measuring agent, tool, prompt, model, and workflow behavior without infecting normal runtime crates.

## Boundary Rule

Eval crates are edge crates. They may depend on Bcode runtime/domain crates to execute and observe behavior, but no core/runtime/domain crate may depend on `bcode_eval` or `bcode_eval_models`.

Allowed dependency direction:

```text
bcode_cli -> bcode_eval -> bcode_eval_models
                         -> bcode_tool / bcode_model / bcode_session_models / ...
```

Forbidden dependency direction:

```text
bcode_tool / bcode_model / bcode_session / bcode_server / bcode_tui -> bcode_eval*
```

Eval-specific concepts live in eval suites, run artifacts, reports, and comparison outputs. If evals need more runtime telemetry, that telemetry must be generally useful in the owning domain and must not mention eval concepts.

## Goals

* Run reproducible suites across cases, variants, and repetitions.
* Compare tools, prompts, models, agent profiles, command workflows, and execution strategies.
* Preserve raw measurements and artifacts so scores can evolve without invalidating old runs.
* Support deterministic CI gates and local exploratory reports.
* Keep suite manifests declarative and reviewable.
* Keep execution, judging, scoring, reporting, and baselining independently extensible.

## Non-goals

* Core crates do not expose eval-specific APIs.
* Eval runs do not mutate normal session stores except through the ordinary public behavior being evaluated.
* Scores are not the source of truth; raw observations and judge results are.

## Crates

### `packages/eval/models`

`bcode_eval_models` owns stable serializable schema types:

* suites, cases, variants, judges, scoring weights, and run configuration
* run manifests and environment snapshots
* observations, measurements, artifacts, diagnostics, and judge results
* repetition/case/variant/run summaries
* comparison, baseline, and regression reports

The crate is intentionally lightweight and has no orchestration logic.

### `packages/eval`

`bcode_eval` owns implementation:

* suite loading, validation, and normalization
* run planning and artifact directory creation
* workspace isolation
* executor dispatch
* judge execution
* measurement collection
* summary, comparison, baseline, and regression reports

### `packages/cli`

The CLI is an edge integration. `bcode eval ...` should be a thin wrapper around `bcode_eval`.

## Suite Model

A suite is made of:

* run defaults
* environment capture settings
* score weights
* variants
* cases

Variants describe what changes between runs: command templates, environment overlays, prompt overlays, model/profile metadata, tool allowlists, or executor-specific settings.

Cases describe the task, fixture, prompt, timeout, case-specific environment, and judges.

## Execution Model

The implemented executor is command execution. It is intentionally general enough to evaluate shell workflows, direct CLIs, or full Bcode agent runs by invoking the existing `bcode` executable with explicit arguments.

Command templates receive concrete environment variables:

* `BCODE_EVAL_RUN_ID`
* `BCODE_EVAL_SUITE_ID`
* `BCODE_EVAL_CASE_ID`
* `BCODE_EVAL_VARIANT_ID`
* `BCODE_EVAL_REPETITION`
* `BCODE_EVAL_WORKSPACE`
* `BCODE_EVAL_PROMPT`
* `BCODE_EVAL_ARTIFACT_DIR`

This keeps the eval package leaf-only while still allowing full product behavior to be evaluated through normal public interfaces.

## Judges

Judges are composable and structured. Implemented judges:

* `exact_diff`: compares the post-run git diff against an expected patch file.
* `command`: runs validation commands in the isolated workspace.
* `file_snapshot`: compares a workspace file to an expected file.
* `regex`: asserts a regex appears or does not appear in a target file, stdout, stderr, or diff.
* `metric_threshold`: enforces numeric metric bounds.

Each judge emits a structured result with pass/fail, optional score, diagnostics, measurements, and artifacts.

## Measurements

Measurements are raw, durable, and namespaced by string keys. Standard metrics include:

* `wall_time_ms`
* `command_exit_code`
* `stdout_bytes`
* `stderr_bytes`
* `diff_bytes`
* `diff_files_changed`
* `diff_additions`
* `diff_deletions`
* judge-specific timing and pass/fail metrics

Future agent/session adapters should map existing provider/tool/session telemetry into eval measurements without changing core types.

## Artifact Layout

Runs are stored under an output root, defaulting to `target/bcode-evals/runs`.

```text
<run-id>/
  run.json
  suite.snapshot.toml
  events.jsonl
  summary.json
  summary.md
  cases/<case-id>/variants/<variant-id>/repetitions/<n>/
    result.json
    observations.jsonl
    metrics.json
    diff.patch
    stdout.log
    stderr.log
    workspace/
```

The layout is intentionally database-free so failures are inspectable with ordinary tools.

## Scoring and Comparison

Correctness should usually gate success. Aggregate reports preserve per-judge pass/fail and raw metrics, then compute weighted variant scores from correctness, efficiency, speed, cost, and stability dimensions.

Comparison reports identify:

* winner by weighted score
* per-variant pass rates and averages
* per-case regressions
* metric deltas
* newly failing/flaky cases

## Baselines and Regressions

A baseline records a selected run for a suite. Regression checks compare a new run with a baseline and report correctness drops, metric increases, and new failures. Baseline files are sidecars under the eval output root and do not affect normal runtime state.
