---
name: verify-prompt-cache
description: Verify that a model provider's prompt caching works as Bcode's catalog claims, using the offline and live prompt-cache harnesses. Interactive — runs `bcode model verify-cache` and the prompt-cache eval suite, interprets every scenario and measurement, and traces failures to the catalog, planner, adapter, or provider before proposing fixes.
allowed-tools: Read(*), Glob(*), Grep(*), Shell(*), Question(*), Edit(*), Write(*)
---

# Verify Prompt Cache

Use this skill when a user asks whether caching "works" for a model, when adding or changing a
cache-capable catalog entry or provider adapter, when cache-related costs look wrong, or when a
prompt-cache guard or eval fails in CI.

Read `docs/prompt-cache-architecture.md` first. It defines the ownership boundaries this skill
relies on: catalog declares claims, `bcode_prompt_cache` derives expectations and plans breakpoints,
provider plugins own wire formats, and one analyzer judges every observation. Never add
model-specific cache logic to `bcode_prompt_cache`; a model that behaves differently declares the
difference in `catalog/models/providers/*.toml`.

## Two mechanisms, two verification paths

Determine the mechanism from the catalog entry (or `bcode model verify-cache --dry-run` output,
which prints `cache=explicit points` / `cache=automatic prefix`):

| Mechanism | Catalog | Examples | Verify with |
| --- | --- | --- | --- |
| Explicit breakpoints | `explicit_prompt_cache = true`, `prompt_cache_ttl_seconds`, `prompt_cache_min_prefix_tokens` | Anthropic Claude on Bedrock | `verify-cache` **and** the eval suite |
| Automatic prefix | `prompt_cache = true` only | OpenAI / ChatGPT Codex, xAI | `verify-cache` only |

The eval suite compares a `cached` variant against a `control` with `mode = "off"`. For automatic
caches the provider caches regardless of host hints, so `control` reuses as much as `cached` and
its zero-reuse judge fails by design. Do not "fix" that by weakening the suite; skip it for
automatic-prefix models.

## Workflow

### 1. Confirm the environment

* Configuration: the commands use the **configured default provider/model**. Provider
  configurations live under `~/.config/bcode/providers/`; list that directory and read the files
  to find the ones relevant to the provider/model under test (look at `[model.profiles.*]`,
  `provider_plugin_id`, `model_id`, `dialect`, and `[auth.profiles.*]`). Select one with
  `BCODE_CONFIG=<path>`, or a profile within the active config via
  `BCODE_CONFIG_TOML='[model]\nprofile = "<profile>"'`. Ask the user which to use only when
  several plausibly match.
* Credentials: `bcode auth status <provider>` for the auth profile that config names (for pools,
  `bcode auth pool status <pool>`).
* Endpoint overrides such as a unified gateway must be in the provider/auth profile settings
  (`endpoint_url`), not only in shell environment variables; the CLI does not read provider
  endpoint env vars when an auth profile supplies the request context.
* Build a binary with bundled plugins: `cargo build -p bcode --bin bcode --features app,static-bundled-plugins`
  (add `,static-bundled-fake-provider-plugin` for offline runs).

Always dry-run first; it costs nothing and confirms model resolution and auth:

```sh
bcode model verify-cache --dry-run
```

If the target model is missing from the dry run, the problem is model resolution (catalog
membership, `ExpandSupported` policy, auth mode / API surface), not caching. Fix that first.

### 2. Run the offline baseline

Before touching a live provider, prove the harness itself is healthy:

```sh
cargo test -p bcode_prompt_cache            # planner, analyzer, simulator, round-trip
scripts/check-prompt-cache-architecture.sh  # boundary guards
scripts/check-prompt-cache-eval.sh          # real daemon + sessions against the fake cache model
```

If these fail, the regression is in Bcode, not the provider. Fix it before spending live tokens.

### 3. Run the live scenario suite

```sh
bcode model verify-cache --id-pattern '<model-id>' --tool-rounds 12 --json --output cache-report.json
```

Omit `--json` for the human table. Non-zero exit means at least one applicable scenario failed
or the suite could not run.

### 4. Run the live eval suite (explicit-breakpoint models only)

```sh
bcode eval run fixtures/evals/prompt-cache/suite.toml
```

Inherits the configured default model. Read `summary.md` / `summary.json` in the run directory;
per-repetition measurements live under `cases/<case>/variants/<variant>/repetitions/0001/`.

## Reading the output

### `verify-cache` scenarios

| Scenario | Passing means | Typical failure cause |
| --- | --- | --- |
| `cold_request` | fresh salted prefix reports no cache reads; every planned point accounted, none dropped | dropped points → planner exceeded provider budget; cached tokens on cold → provider caches by content and the prefix salt is missing |
| `warm_same_prefix` | identical repeat reads ≥ `min_warm_read_ratio` (0.5) of input; explicit models wrote on the cold request | no reads → hints not serialized, unstable prefix (timestamps, ids), or `prompt_cache_key` missing |
| `growing_conversation` | cached tokens never shrink as turns append | breakpoints not rolling forward; provider re-keying |
| `tool_loop` | hit ratio ≥ 0.9, `late_uncached_ratio` ≤ 0.15, `write_amplification` ≤ 3 | tool results not covered by a point (explicit); prefix mutated per round |
| `ttl_matrix` | every advertised TTL accepted and echoed; unadvertised TTL rejected as `UnsupportedFeature` | catalog TTLs disagree with the adapter's mapping or the provider |
| `mode_off` | no points emitted, no writes when caching is off | planner or adapter ignores `PromptCacheMode::Off` |
| `budget_overflow` | over-budget points dropped **with accounting**, not an error | adapter lacks `emitted/dropped_cache_point_count` or errors instead of trimming |

Skips are not passes. `ttl_matrix` and `budget_overflow` skip for automatic-prefix models; every
other skip means a capability is not advertised and should be investigated.

### Measurement keys (`prompt_cache.*`)

Defined once in `bcode_prompt_cache_models::measurement`; identical in `verify-cache` reports and
eval `summary.json`:

* `hit_round_ratio` — eligible rounds (all after the first) with `cached_input_tokens > 0`.
* `cached_input_increase_count` — consecutive rounds where cached tokens grew; rolling points work.
* `uncached_input_tokens` — ordinary (neither read nor written) input; the cost that caching removes.
* `late_uncached_ratio` — `uncached / input` over the final third of rounds; the steady state.
* `write_amplification` — `Σ cache writes / final round input`. ~1.0 means each prefix segment was written once; > 3 means churn. Always 0 for providers that do not report writes.
* `warm_read_ratio` — `cached / input` on the same-prefix repeat.
* `dropped_cache_points` — must be 0 under normal planning.

### Eval suite

`passed: true` requires the `cached` judges (hit ratio, late tail, write amplification,
post-restart `follow_up.prompt_cache.cached_input_tokens ≥ 1`), the `control` zero-reuse judge, and
the run-level comparison `cached / control prompt_cache.uncached_input_tokens ≤ 0.35`. A ratio near
1.0 means no reuse at all; between 0.35 and 1.0 means partial reuse — inspect `hit_round_ratio`
and `late_uncached_ratio` per round to see where it stops.

## Diagnosing failures

Work from the outside in and stop at the first layer that is wrong:

1. **Catalog claims** (`catalog/models/providers/*.toml`): is `explicit_prompt_cache`, the TTL set,
   and `prompt_cache_min_prefix_tokens` right for this model? `cargo test -p bcode_model_catalog`
   validates TTL/pricing agreement. Wrong claims make the planner and the suite expect the wrong
   behavior; fix them here, never in code.
2. **Host planner** (`packages/prompt-cache/src/planning.rs`): `dropped_cache_points > 0` or
   missing points → budget or min-prefix logic. Reproduce with the fake provider first
   (`cargo test -p bcode_prompt_cache`); the simulator is the reference.
3. **Provider adapter** (`plugins/*-provider-plugin`): hints accepted but no reads → check that
   `cache_control` / `prompt_cache_key` serialize, that usage parsing maps `cached_tokens` /
   `cache_read_input_tokens` / `cache_creation_input_tokens` into `TokenUsage`, and that request
   projection reports point counts. A contract violation (`turn ended with Error …`) names the
   adapter error code; the live suite has caught adapter bugs before (empty tool deltas, missing
   `max_output_tokens`).
4. **Provider behavior**: everything above is correct but reuse is low → the request prefix is
   genuinely unstable (dynamic system prompt sections, per-request ids, tool ordering). Compare two
   consecutive request projections; the daemon's `model_request_built` trace event shows
   `prompt_cache_points`, `cache_system_prompt`, and `cache_tools`.

Do not loosen thresholds in `PromptCacheThresholds` or the eval fixture to make a run pass. If a
provider legitimately behaves differently, express it as a capability claim and derive the
expectation from that claim.

## Making improvements

* A new cache-capable model: add the catalog entry with all four cache fields and TTL-priced
  `cache_write_input` rules, run `cargo test -p bcode_model_catalog`, then `verify-cache` live.
* A new provider adapter: it must pass `run_provider_conformance_suite` (the `prompt caching` case)
  and `run_prompt_cache_scenarios` against a deterministic test double before live verification.
  Follow `packages/prompt-cache/tests/fake_provider_round_trip.rs`.
* A planner change: add the case to `planning.rs` tests, confirm the round-trip test still passes,
  then rerun `scripts/check-prompt-cache-eval.sh` — it exercises the planner inside a real daemon.
* Record live numbers (warm read ratio, tool-loop hit ratio, uncached-vs-control ratio) in the
  model's doc note (see `docs/bedrock-fable-5-1.md`) so future runs have a baseline to compare.

Report exactly which commands ran, the per-scenario outcomes, the key measurements, and the layer
any failure was traced to.
