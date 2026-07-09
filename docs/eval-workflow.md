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
public daemon plugin-service API. Use it to measure tool behavior independent of
model/tool-choice behavior.

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
