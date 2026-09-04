# Prompt Cache Architecture

Bcode treats provider prompt caching as a first-class domain: how a model's cache is expected to
behave is derived from normalized capability claims, the host plans cache breakpoints from those
same claims, and observed behavior is judged by one analyzer regardless of whether the observation
came from a live provider, a test double, or persisted session history.

## Ownership

| Concern | Owner |
| --- | --- |
| Declaring what a model's cache can do | `catalog/models/providers/*.toml` → `bcode_model_catalog` → `ModelCacheInfo` |
| Expected behavior derived from claims | `bcode_prompt_cache::expectations` |
| Host-side breakpoint planning | `bcode_prompt_cache::planning` (called by `bcode_server`) |
| Judging observed rounds | `bcode_prompt_cache::analysis` (the only place cache-ratio math lives) |
| Driving a provider through cache workloads | `bcode_prompt_cache::scenarios` |
| Deterministic reference cache | `bcode_prompt_cache::simulation` (feature `simulation`) |
| Portable types shared by all of the above | `bcode_prompt_cache_models` |
| Wire formats (`cache_control`, `prompt_cache_key`, point budgets) | provider plugins |

Provider plugins never become inputs to expectation derivation: no code in `bcode_prompt_cache`
matches model identifiers. A model that behaves differently declares the difference in the
catalog.

## Capability claims

`CatalogCapabilities` carries four cache fields:

* `prompt_cache` — the provider discounts or controls cached prefixes at all.
* `explicit_prompt_cache` — the caller marks breakpoints (Anthropic-style). When false and
  `prompt_cache` is true the cache is automatic-prefix (OpenAI-style).
* `prompt_cache_ttl_seconds` — explicit TTLs the model accepts.
* `prompt_cache_min_prefix_tokens` — the shortest prefix the provider will cache. Required when
  `explicit_prompt_cache = true`, because breakpoints on shorter prefixes are never reusable.

`validate_catalog` rejects TTLs priced in `cache_write_input` rules that are not advertised, and
advertised TTLs that carry no TTL-specific price when any TTL is priced.

These normalize into `ModelCacheInfo { capabilities, ttl_seconds, min_prefix_tokens }`, which is
what the planner, the scenario suite, and the eval executor consume.

## Planning

`plan_prompt_cache` removes stale points, then for explicit-cache models places up to
`MAX_CONVERSATION_CACHE_POINTS` conversation points on completed user messages and tool results,
excluding the mutable tail so the newest point advances each round. Two rules keep points useful:

* a point is placed only when the estimated prefix through it (system prompt + tools + messages)
  reaches `min_prefix_tokens`, falling back to `DEFAULT_MIN_PREFIX_TOKENS` (1024) when the
  catalog does not say;
* the conversation budget is `DEFAULT_MAX_CACHE_POINTS` minus one for the system prompt and one
  for tool definitions when present, so a request never exceeds the provider's per-request limit
  and provider-side drops indicate a real bug rather than expected trimming.

Automatic-prefix models receive hints (`mode`, `key`, `cache_system_prompt`, `cache_tools`) but no
message points.

## Verification

`run_prompt_cache_scenarios` works over any `BlockingModelProviderInvoker` and reports a versioned
`PromptCacheVerificationReport`:

| Scenario | Checks |
| --- | --- |
| `cold_request` | fresh key reports no reads; points accounted; no drops |
| `warm_same_prefix` | identical repeat reads ≥ `min_warm_read_ratio` of input |
| `growing_conversation` | cached tokens never shrink across appended turns |
| `tool_loop` | hit ratio, monotonic reads, bounded late uncached tail, bounded write amplification |
| `ttl_matrix` | each advertised TTL accepted and echoed; unadvertised TTL fails closed |
| `mode_off` | no points, no writes with caching disabled |
| `budget_overflow` | over-budget points dropped with accounting, not an error |

Scenarios inapplicable to the advertised mechanism are reported as skipped, never passed.
Thresholds live in `PromptCacheThresholds` with conservative defaults and are overridable.

Three consumers run the suite:

* `packages/prompt-cache/tests/fake_provider_round_trip.rs` runs it against the fake provider's
  `fake-cache-explicit` and `fake-cache-prefix` models, which are backed by the reference
  simulator. This is the CI proof that planner, scenarios, analyzer, and simulator agree.
* `packages/model-provider-runtime/tests/fake_provider_conformance.rs` requires the public
  provider conformance suite's `prompt caching` case to pass for those models.
* `bcode model verify-cache` runs it against the configured live provider (credentials required)
  and prints a per-scenario table or, with `--json` / `--output`, a versioned report envelope
  containing every model's `PromptCacheVerificationReport`. It exits non-zero when any applicable
  scenario fails or a model's suite cannot run. Use `--id-pattern` to select models,
  `--tool-rounds` / `--conversation-turns` to lengthen workloads, and `--min-prefix-tokens` to
  supply a minimum for models whose catalog entry omits one.

  ```sh
  bcode model verify-cache --id-pattern 'global.anthropic.claude-fable-5-1' --tool-rounds 12
  ```

## Analysis

`analyze_rounds` and `analyze_warm_repeat` take `CacheRoundObservation`s built from normalized
`TokenUsage` (via `uncached_input_tokens()` / `has_valid_input_breakdown()`) and
`ProviderRequestProjection`. `CacheRoundObservation::from_session_usage` builds the same
observation from persisted session usage so eval telemetry and live verification judge identical
numbers. Measurements use stable string keys from `bcode_prompt_cache_models::measurement` so they
can flow into eval artifacts unchanged.

## End-to-end eval

`fixtures/evals/prompt-cache/suite.toml` runs a real Bcode session through a twelve-file
`filesystem.read` loop twice: `cached` with `[model.prompt_cache] mode = "auto"` and `control`
with `mode = "off"`, via per-variant `config_toml` overlays on isolated daemons. The `cached`
variant also restarts its daemon and sends a same-session follow-up, which must still hit the
cache. Judges assert hit ratio, late-tail, write amplification, and post-restart reuse for
`cached`, zero reuse for `control`, and a run-level `[[comparisons]]` entry requires cached
uncached-input to be at most 35% of control.

The suite is model-agnostic across explicit-breakpoint caches: point the daemon configuration at
any such provider/model. Automatic-prefix providers cache regardless of host hints, so their
`control` variant reuses as much as `cached`; verify those with `bcode model verify-cache`, whose
`mode_off` scenario only asserts what the host controls. CI runs the suite against the fake
provider's `fake-cache-explicit` model (`scripts/check-prompt-cache-eval.sh`), which persists its
simulated cache beneath `BCODE_STATE_DIR` so a restarted daemon sees the same entries a real
provider would. Live runs use the same suite against a credentialed model:

```sh
bcode eval run fixtures/evals/prompt-cache/suite.toml
```

## Adding a cache-capable model

1. Declare `prompt_cache`, `explicit_prompt_cache`, `prompt_cache_ttl_seconds`, and
   `prompt_cache_min_prefix_tokens` in the provider catalog entry; add TTL-specific
   `cache_write_input` pricing rules.
2. `cargo test -p bcode_model_catalog` — validation catches inconsistent declarations.
3. Run the live scenario suite against the model and fix adapter behavior until every applicable
   scenario passes.

## Related documents

* `docs/model-provider-contract.md` — conformance and capability truthfulness
* `docs/eval-architecture.md` — eval boundary rules the cache eval suite follows
