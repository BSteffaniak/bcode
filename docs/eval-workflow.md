# Eval Workflow

Bcode evals are edge-level tooling for comparing prompts, models, tools, and
agent policies without making core crates depend on eval behavior.

## Self-improvement campaigns

Eval improvement campaigns record multi-generation attempts to improve a suite
while preserving every delta and metric shift for review.

```bash
cargo run -p bcode -- eval improve start \
  fixtures/evals/edit-tools/suite.toml \
  --campaign-id edit-tools-self-improve \
  --baseline-run target/bcode-evals/runs/edit-tools-local
```

A campaign is stored under `target/bcode-evals/improvements/<campaign-id>/` and
contains a baseline generation plus later generations. Each generation records
its parent, branch, delta summary, optional patch, optional eval run, metric
deltas against parent/baseline, and verdict.

Record a generation after testing a change:

```bash
cargo run -p bcode -- eval improve record \
  target/bcode-evals/improvements/edit-tools-self-improve \
  --kind system_prompt_overlay \
  --summary "Prefer inspecting relevant files before editing" \
  --run target/bcode-evals/runs/edit-tools-gen-0001 \
  --risk low
```

Inspect the campaign timeline:

```bash
cargo run -p bcode -- eval improve status \
  target/bcode-evals/improvements/edit-tools-self-improve
```

Supported delta kinds include `system_prompt_overlay`, `system_prompt_patch`,
`tool_description_overlay`, `tool_schema_patch`, `tool_behavior_patch`,
`agent_profile_overlay`, `permission_policy_overlay`, `model_change`,
`eval_case_change`, `judge_change`, `scoring_change`, and `mixed`.

The current implementation establishes the durable campaign/generation history
that future LLM diagnosis, automated loops, branching, promotion, and TUI
campaign views can build on.

## Common commands

```bash
cargo run -p bcode -- eval validate \
  fixtures/evals/edit-tools/suite.toml
cargo run -p bcode -- eval run \
  fixtures/evals/edit-tools/suite.toml \
  --run-id edit-tools-local \
  --fail-under-pass-rate 1.0
cargo run -p bcode -- eval compare \
  target/bcode-evals/runs/edit-tools-local \
  --markdown target/bcode-evals/runs/edit-tools-local/comparison.md
```

## Agent executor

`executor = "agent"` creates a real Bcode session, sends the case prompt, waits
for the model turn to finish, and captures session-derived telemetry:

* transcript JSONL
* tool-call JSONL
* token usage
* tool counts
* permission prompts
* tool errors
* wall time
* optional cost estimates

Agent eval repetitions use isolated daemon state by default:

* `BCODE_STATE_DIR` points at the repetition artifact directory
* `BCODE_SOCKET` points at the repetition artifact directory
* `BCODE_PERMISSIONS_STATE` points at a generated permissions overlay when
  `allowed_tools` is configured

Set variant metadata `daemon_isolation = "shared"` only for local debugging.
Shared mode can reuse an already-running daemon and may not enforce eval policy.

### Configuration overlays

`config_toml` applies a TOML overlay to the isolated daemon for one variant. It
travels through the daemon's environment (`BCODE_CONFIG_TOML`) only, so it never
creates or modifies declarative configuration files, and it is rejected together
with `daemon_isolation = "shared"`. Use it to compare runtime settings such as
prompt-cache mode across otherwise identical variants:

```toml
[[variants]]
id = "cached"
executor = "agent"
config_toml = """
[model.prompt_cache]
mode = "auto"
"""
```

### Follow-up turns and daemon restart

`[variants.follow_up]` sends a second prompt on the same session after the main
turn completes. Its telemetry lands under `follow_up.*` measurements and a
`follow-up-transcript.jsonl` artifact. With `restart_daemon = true` the isolated
daemon is stopped and started again first, so the follow-up exercises session
resume through normal client boundaries:

```toml
[variants.follow_up]
prompt = "Reply with exactly: follow-up complete."
restart_daemon = true
```

### Prompt-cache telemetry

Every agent repetition also reports per-round prompt-cache measurements derived
by `bcode_prompt_cache::analysis` from the session's `model_usage` events
(`prompt_cache.hit_round_ratio`, `prompt_cache.uncached_input_tokens`,
`prompt_cache.late_uncached_ratio`, `prompt_cache.write_amplification`, and so
on). These are the same measurements `bcode model verify-cache` reports, so a
threshold that holds in live verification can be asserted in an eval unchanged.
See `docs/prompt-cache-architecture.md` and `fixtures/evals/prompt-cache/`.

### Variant-scoped metric judges

`metric_threshold` judges accept `variants = ["id", ...]`. The judge applies
only to those variants and is recorded as passed-and-not-required for others,
so a variant-specific expectation (cache reuse) does not fail its control.

## Cross-variant comparisons

`[[comparisons]]` declares run-level assertions between variants of the same
run. Each divides the `variant` measurement by the `baseline_variant`
measurement (both averaged across cases and repetitions) and checks it against
exactly one of `max_ratio` or `min_ratio`. Required comparisons fail the run;
results are persisted in `summary.json` under `comparisons` and rendered in
`summary.md`.

```toml
[[comparisons]]
id = "cached-input-below-control"
metric = "prompt_cache.uncached_input_tokens"
variant = "cached"
baseline_variant = "control"
max_ratio = 0.35
```

Example agent variants:

```toml
[[variants]]
id = "vim-edit-agent"
name = "Vim edit agent"
executor = "agent"
profile = "eval"
allowed_tools = [
  "vim_edit.preview",
  "vim_edit.apply",
  "shell.run",
  "filesystem.read",
]
model = "your-model-id"
metadata = {
  agent_id = "build",
  input_cost_per_million_tokens = 3.0,
  output_cost_per_million_tokens = 15.0,
}

[[variants]]
id = "filesystem-edit-agent"
name = "Filesystem edit agent"
executor = "agent"
profile = "eval"
allowed_tools = ["filesystem.read", "filesystem.edit", "shell.run"]
model = "your-model-id"
metadata = { agent_id = "build" }
```

## Direct-tool executor

`executor = "direct_tool"` invokes a model-callable tool service through the
public daemon plugin-service API. It performs the tool owner's side-effect-free
`prepare_tool` operation first, forwards the resulting opaque descriptor to
`invoke_tool`, and reports preparation latency separately. Use it to measure tool
behavior independent of model/tool-choice behavior.

This is intentionally a raw service harness, not a canonical agent-runtime
adapter: it does not coordinate authorization or provide exchange, input,
nested-service, artifact, or cancellation capability brokers. Tools requiring
those capabilities should be evaluated through `executor = "agent"`.

See `fixtures/evals/direct-tools/suite.toml` for schema examples.

## Replay executor

`executor = "replay"` reads session-event JSONL and computes the same telemetry
without rerunning model calls.

Export an existing session:

```bash
cargo run -p bcode -- eval replay-session \
  <session-id> fixtures/evals/replays/session.jsonl
```

Then point a replay case or variant at that JSONL:

```toml
[[variants]]
id = "historical-session"
executor = "replay"

[variants.replay]
transcript = "replays/session.jsonl"
```

## CI usage

Use pass-rate and regression flags for stable exit behavior:

```bash
cargo run -p bcode -- eval run \
  fixtures/evals/edit-tools/suite.toml \
  --fail-under-pass-rate 1.0
cargo run -p bcode -- eval compare \
  target/bcode-evals/runs/latest \
  --fail-under-pass-rate 1.0
cargo run -p bcode -- eval regressions \
  baseline.json target/bcode-evals/runs/latest \
  --fail-on-regression
```
