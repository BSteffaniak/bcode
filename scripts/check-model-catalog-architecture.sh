#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if grep -R --include='Cargo.toml' -n 'bcode_model_catalog' plugins/*/Cargo.toml; then
  echo "provider plugins must not depend on bcode_model_catalog" >&2
  exit 1
fi

if grep -R --include='*.rs' -nE 'load_bundled|RemoteCatalogClient|load_bundled_with_remote_overlay' plugins/*-provider-plugin/src; then
  echo "provider plugins must not load or fetch model catalogs" >&2
  exit 1
fi

if grep -R --include='*.rs' -n 'ensure_selected_model_info' plugins/*-provider-plugin/src; then
  echo "provider plugins must not insert selected models" >&2
  exit 1
fi

if grep -R --include='*.rs' -nE 'gpt-5\.5|gpt-5\.6-sol' plugins/*-provider-plugin/src \
  | grep -vE ':[0-9]+:.*(test|build_responses_request|Some\(|model_id = )'; then
  echo "provider plugins must not contain catalog-owned model defaults" >&2
  exit 1
fi

if grep -R --include='*.rs' -nE 'pricing::fetch_region|agreement_pricing|pricing_from_agreement_rates' \
  plugins/*-provider-plugin/src; then
  echo "provider plugins must not fetch or normalize model pricing catalogs" >&2
  exit 1
fi
if grep -R --include='*.rs' -nE 'gpt-5\.6.*(price|cost)|price.*gpt-5\.6' \
  plugins/*-provider-plugin/src; then
  echo "provider plugins must not own model-specific pricing thresholds or rates" >&2
  exit 1
fi

if grep -nE 'pricing\.amazonaws\.com|priceDimensions|pricePerUnit|function .*pricing.*rule' \
  workers/models-catalog/src/worker.js; then
  echo "the models-catalog Worker may consume normalized pricing but must not fetch or normalize AWS pricing catalogs" >&2
  exit 1
fi

if ! grep -F "new Request('https://assets.invalid/v1/live/bedrock.json')" \
  workers/models-catalog/src/worker.js >/dev/null; then
  echo "the Bedrock Worker refresh must consume the normalized static pricing seed" >&2
  exit 1
fi

if ! grep -F 'rules: catalog_pricing_rules(pricing)' packages/model-catalog/src/lib.rs >/dev/null; then
  echo "conditional pricing rules must resolve through the model catalog" >&2
  exit 1
fi

if grep -R --include='*.rs' -nE 'fn (effective_model_id|resolve_model_api_surface)' packages/server/src; then
  echo "server model identity and API surface must resolve together through model_request_target" >&2
  exit 1
fi

if ! grep -F 'resolve_model_request_target(' packages/server/src/context_compaction.rs >/dev/null ||
   ! grep -F 'resolve_model_request_target(' packages/server/src/lib.rs >/dev/null; then
  echo "production model request paths must use centralized request-target resolution" >&2
  exit 1
fi

if ! grep -F 'discovered_xai_language_models_advertise_documented_tool_capabilities' packages/model-discovery/src/xai.rs >/dev/null ||
   ! grep -F 'xai_model_candidates_advertise_documented_tool_capabilities' plugins/openai-compatible-provider-plugin/src/lib.rs >/dev/null; then
  echo "xAI documented tool and parallel-tool capability coverage was removed" >&2
  exit 1
fi

count="$(grep -c 'invoke_model_provider_json_blocking::<_, ModelList>' packages/server/src/lib.rs)"
if [[ "$count" != "1" ]]; then
  echo "expected exactly one direct server OP_MODELS invocation, found $count" >&2
  exit 1
fi

if rg -n 'fn (native_web_search|native_web_search_inner)|impl .*NativeWebSearch' packages \
  >/tmp/bcode-host-native-web-search-implementation.txt; then
  echo "provider-native web search implementation must remain behind provider plugin interfaces" >&2
  cat /tmp/bcode-host-native-web-search-implementation.txt >&2
  exit 1
fi

native_search_implementations="$(
  rg -l 'fn (native_web_search|native_web_search_inner)' plugins/*-provider-plugin/src/lib.rs | sort
)"
expected_native_search_implementations="$(cat <<'EOF'
plugins/fake-provider-plugin/src/lib.rs
plugins/openai-compatible-provider-plugin/src/lib.rs
EOF
)"
if [[ "$native_search_implementations" != "$expected_native_search_implementations" ]]; then
  echo "provider-native web search implementations moved outside the audited provider plugins" >&2
  diff -u <(printf '%s\n' "$expected_native_search_implementations") <(printf '%s\n' "$native_search_implementations") >&2 || true
  exit 1
fi

if sed -n '1,19720p' packages/server/src/lib.rs | rg -n 'OP_NATIVE_WEB_SEARCH|NativeWebSearch(Request|Response)|model.native|model_native' \
  >/tmp/bcode-server-native-search-production.txt; then
  echo "server production routing must remain opaque to provider-native web search" >&2
  cat /tmp/bcode-server-native-search-production.txt >&2
  exit 1
fi

if ! grep -F 'invocation_operations = ["native_web_search"]' plugins/fake-provider-plugin/bcode-plugin.toml >/dev/null ||
   ! grep -F 'invocation_operations = ["native_web_search"]' plugins/openai-compatible-provider-plugin/bcode-plugin.toml >/dev/null; then
  echo "provider-native invocation service exports must remain manifest-declared" >&2
  exit 1
fi

if rg -n '(model_id|effective_model_id)[^\n]*(contains|starts_with|ends_with)\(' \
  packages/server/src plugins/prompt-profile-plugin/src \
  >/tmp/bcode-prompt-profile-model-id-matching.txt; then
  echo "prompt profiles must use exact catalog-resolved identity, not model-id substring matching" >&2
  cat /tmp/bcode-prompt-profile-model-id-matching.txt >&2
  exit 1
fi

if ! grep -F 'model_identity(' packages/server/src/model_request_target.rs >/dev/null; then
  echo "prompt profile identity must come from centralized model request-target resolution" >&2
  exit 1
fi

if rg -n 'context_threshold_tokens|ModelPricingRule|price_bucket_micros' \
  packages/server/src packages/tui/src/app.rs packages/session/src packages/session-view/src --glob '*.rs'; then
  fail "pricing thresholds, rule selection, and rate application must remain model/catalog-owned"
fi
if ! rg -n 'pricing_from_catalog' packages/model-catalog/src/lib.rs >/dev/null; then
  fail "catalog pricing normalization must remain in the model-catalog domain"
fi

echo "model catalog architecture guard passed"
