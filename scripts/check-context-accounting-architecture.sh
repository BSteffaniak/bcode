#!/usr/bin/env bash
set -euo pipefail

fail() { echo "context accounting architecture guard failed: $*" >&2; exit 1; }

if rg -n 'context_input_tokens|active_context_tokens' packages/model/src plugins --glob '*.rs'; then
  fail "billing TokenUsage/provider adapters must not carry context occupancy fields"
fi
if rg -n 'total_tokens.*context|context.*total_tokens' packages/server/src plugins --glob '*.rs'; then
  fail "total token metering must not drive request context occupancy"
fi
if rg -n 'provider_turn_id' packages/session/models/src/context_management.rs; then
  fail "provider turn ids must remain runtime routing state"
fi
if rg -n 'latest_context_input_tokens|context_usage_sequence|context_usage_estimated' packages/tui/src/app.rs; then
  fail "TUI must store RequestContextOccupancy directly"
fi
if rg -n 'provider_state\.as_ref\(\)' packages/server/src/lib.rs; then
  fail "opaque provider continuation state must not participate in local estimation"
fi
if rg -n 'local_model_request_estimate_tokens' packages/server/src/context_compaction.rs; then
  fail "compaction must consume PreparedModelRequest projections"
fi

complete_input_sources="$(rg -l 'fn complete_request_input_tokens' plugins/*-provider-plugin/src --glob '*.rs' || true)"
if [[ "$complete_input_sources" != "plugins/bedrock-provider-plugin/src/lib.rs" ]]; then
  fail "complete provider-visible request input must have one audited semantic source"
fi
if rg -n 'input_tokens\s*\+\s*(cached_input_tokens|cache_(read|write)_input_tokens)' \
  packages/server/src packages/session/src packages/session-view/src packages/tui/src --glob '*.rs'; then
  fail "hosts and renderers must not independently reconstruct complete provider-visible input"
fi

echo "context accounting architecture guard passed"
