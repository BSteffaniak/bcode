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

count="$(grep -c 'invoke_model_provider_json_blocking::<_, ModelList>' packages/server/src/lib.rs)"
if [[ "$count" != "1" ]]; then
  echo "expected exactly one direct server OP_MODELS invocation, found $count" >&2
  exit 1
fi

echo "model catalog architecture guard passed"
