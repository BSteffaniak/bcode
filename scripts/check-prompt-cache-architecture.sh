#!/usr/bin/env bash
set -euo pipefail

# Guards for the prompt-cache domain described in docs/prompt-cache-architecture.md.

fail() { echo "prompt cache architecture guard failed: $*" >&2; exit 1; }

# 1. Expectations, planning, analysis, and simulation derive from capability claims, never from
#    model identifiers. Provider plugins and the catalog are the only places allowed to know
#    which models exist.
if rg -n 'model_id[^\n]*(contains|starts_with|ends_with|==\s*")|is_claude|is_gpt|fable|anthropic\.|openai\.' \
  packages/prompt-cache/src packages/prompt-cache/models/src --glob '*.rs'; then
  fail "bcode_prompt_cache must derive behavior from ModelCacheInfo/feature claims, not model identifiers"
fi

# 2. Cache-ratio arithmetic has one home. Hosts, renderers, evals, and the CLI consume
#    bcode_prompt_cache analysis or TokenUsage helpers instead of re-deriving hit ratios.
if rg -n '(cached_input_tokens|cache_write_input_tokens)[^\n]*\s/\s*[^\n]*input_tokens' \
  packages/server/src packages/cli/src packages/eval/src packages/tui/src packages/session-view/src \
  packages/hyperchad/ui/src --glob '*.rs'; then
  fail "cache hit ratios must come from bcode_prompt_cache::analysis, not ad hoc division"
fi

# 3. The host planner lives in the prompt-cache domain; the server only calls it.
if rg -n 'fn plan_prompt_cache|fn conversation_cache_point_indices' packages/server/src --glob '*.rs'; then
  fail "prompt-cache planning must live in bcode_prompt_cache::planning"
fi
if ! rg -q 'bcode_prompt_cache::planning::plan_prompt_cache' packages/server/src/lib.rs; then
  fail "the server request builder must plan cache points through bcode_prompt_cache"
fi

# 4. Every bundled provider plugin that advertises PromptCaching exercises the cache conformance
#    case through a test double, so cache regressions are caught without credentials.
if ! rg -q 'prompt caching' packages/model-provider-runtime/tests/fake_provider_conformance.rs; then
  fail "the fake provider conformance test must require the prompt caching case"
fi
if ! rg -q 'run_prompt_cache_scenarios' packages/prompt-cache/tests/fake_provider_round_trip.rs; then
  fail "the prompt-cache round-trip test must run the scenario suite against the fake cache models"
fi
# The live entry point is a thin adapter over the same suite; it must not grow its own checks.
if ! rg -q 'bcode_prompt_cache::scenarios::run_prompt_cache_scenarios' packages/cli/src/lib.rs; then
  fail "bcode model verify-cache must run bcode_prompt_cache::scenarios rather than a CLI-local suite"
fi

# 5. Explicit-cache catalog entries declare their minimum cacheable prefix so the planner never
#    spends breakpoints on unhittable prefixes. Enforced at load time; keep the guard in sync.
if ! rg -q 'prompt_cache_min_prefix_tokens' packages/model-catalog/src/lib.rs; then
  fail "catalog validation must require prompt_cache_min_prefix_tokens for explicit-cache entries"
fi

# 6. The prompt-cache crates stay renderer- and daemon-neutral.
if rg -n 'bcode_tui|bcode_server|bcode_client|bcode_ipc|ratatui|bcode_eval' \
  packages/prompt-cache/Cargo.toml packages/prompt-cache/models/Cargo.toml; then
  fail "bcode_prompt_cache must not depend on renderers, the daemon, IPC, or evals"
fi

echo "prompt cache architecture guard passed"
