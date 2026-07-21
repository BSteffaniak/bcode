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
  | grep -vE ':[0-9]+:.*(test|build_responses_request|Some\()'; then
  echo "provider plugins must not contain catalog-owned model defaults" >&2
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

count="$(grep -c 'OP_NATIVE_WEB_SEARCH,' packages/server/src/lib.rs)"
if [[ "$count" != "2" ]]; then
  echo "expected one server provider-interface native search route plus its import, found $count references" >&2
  exit 1
fi

echo "model catalog architecture guard passed"
