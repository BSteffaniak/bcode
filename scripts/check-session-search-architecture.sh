#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

fail() {
  echo "session-search architecture guard failed: $*" >&2
  exit 1
}

contract_manifest="packages/session-search/Cargo.toml"
contract_source="packages/session-search/src"

[[ -f "$contract_manifest" ]] || fail "missing backend-neutral session-search contract manifest"
[[ -d "$contract_source" ]] || fail "missing backend-neutral session-search contract source"

# Shared session-search contracts are portable leaves. They may use session-owned models and
# serialization/hash utilities, but not application hosts, renderers, plugin runtimes, persistence
# engines, or concrete search backends.
for forbidden_dependency in \
  bcode_server \
  bcode_tui \
  bcode_client \
  bcode_ipc \
  bcode_plugin \
  bcode_plugin_sdk \
  bcode_session \
  switchy \
  rusqlite \
  sqlx \
  turso \
  tantivy; do
  if grep -Eq "^[[:space:]]*${forbidden_dependency}[[:space:]]*=" "$contract_manifest"; then
    fail "shared contracts depend on forbidden implementation crate '${forbidden_dependency}'"
  fi
done

if rg -n \
  'bcode_(server|tui|client|ipc|plugin|session)::|switchy::|rusqlite::|sqlx::|turso::|tantivy::' \
  "$contract_source" --glob '*.rs' \
  >/tmp/bcode-session-search-contract-boundary.txt; then
  cat /tmp/bcode-session-search-contract-boundary.txt >&2
  fail "shared contracts import an application, persistence, plugin-runtime, renderer, or backend implementation"
fi

# Any concrete provider advertising the session-search service must consume only the portable
# contract/SDK boundary. Canonical session persistence and its implementation vocabulary remain
# unavailable even if a new provider is added without updating this guard.
provider_manifests=()
while IFS= read -r manifest; do
  provider_manifests+=("$manifest")
done < <(rg -l 'interface_id[[:space:]]*=[[:space:]]*"bcode\.session_search/v1"' \
  plugins --glob 'bcode-plugin.toml' | sort)

for manifest in "${provider_manifests[@]}"; do
  provider_dir="$(dirname "$manifest")"
  cargo_manifest="$provider_dir/Cargo.toml"
  [[ -f "$cargo_manifest" ]] || fail "provider '$provider_dir' has no Cargo.toml"

  if rg -n \
    '^[[:space:]]*(bcode_session|switchy|rusqlite|sqlx|turso)[[:space:]]*=' \
    "$cargo_manifest" >/tmp/bcode-session-search-provider-dependencies.txt; then
    cat /tmp/bcode-session-search-provider-dependencies.txt >&2
    fail "provider '$provider_dir' depends on canonical session persistence or a database implementation"
  fi

  if rg -n \
    'bcode_session::|switchy::|rusqlite::|sqlx::|turso::|catalog\.db|session\.db|session_db_path|session_dir_path' \
    "$provider_dir/src" --glob '*.rs' \
    >/tmp/bcode-session-search-provider-storage.txt; then
    cat /tmp/bcode-session-search-provider-storage.txt >&2
    fail "provider '$provider_dir' opens or understands canonical session persistence"
  fi
done

# The optional Tantivy backend must remain absent from ordinary/default products and enter the
# feature graph only through its independently named static-bundle feature.
if cargo tree -p bcode_bundled_plugins --no-default-features -i tantivy 2>/dev/null \
  | grep -q '^tantivy '; then
  fail "Tantivy enters the bundled-plugin graph when its feature is disabled"
fi
if ! cargo tree -p bcode_bundled_plugins --no-default-features \
  --features static-bundled-tantivy-session-search-plugin -i tantivy 2>/dev/null \
  | grep -q '^tantivy '; then
  fail "Tantivy is absent when its explicit static-bundle feature is enabled"
fi
if cargo tree -p bcode --no-default-features -i tantivy 2>/dev/null | grep -q '^tantivy '; then
  fail "Tantivy enters the product facade when its feature is disabled"
fi

# Application coordination stays at the server boundary; the generic plugin host must not acquire
# session-search planning, projection, routing, hydration, or backend policy.
if rg -n \
  'bcode_session_search|SESSION_SEARCH_INTERFACE_ID|SessionSearch(Request|Plan|ContentRoute)|SearchContentKind' \
  packages/plugin packages/plugin-sdk --glob '*.rs' --glob 'Cargo.toml' \
  >/tmp/bcode-session-search-plugin-host-semantics.txt; then
  cat /tmp/bcode-session-search-plugin-host-semantics.txt >&2
  fail "generic plugin infrastructure contains session-search domain semantics"
fi

printf 'session-search architecture guard passed (%d concrete provider manifests checked)\n' \
  "${#provider_manifests[@]}"
