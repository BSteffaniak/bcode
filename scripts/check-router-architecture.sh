#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

model_manifests=(
  packages/router/api/models/Cargo.toml
  packages/router/catalog/models/Cargo.toml
  packages/router/config/models/Cargo.toml
  packages/router/introspection/models/Cargo.toml
  packages/router/models/Cargo.toml
  packages/router/provider/models/Cargo.toml
  packages/router/telemetry/models/Cargo.toml
)

for manifest in "${model_manifests[@]}"; do
  if rg -n '^(brouter_(catalog|config|introspection|provider|router|server|telemetry)|axum|reqwest|sshenv_vault|switchy_database|switchy_database_connection|tower|tower-http)\s*=' "$manifest"; then
    echo "router model crates must depend only on portable contracts and serialization support: $manifest" >&2
    exit 1
  fi
done

if rg -n '^(bcode_(agent_runtime|server|tui)|brouter_(catalog|config|introspection|provider|server|telemetry))\s*=' packages/router/Cargo.toml; then
  echo "the routing engine must not depend on Bcode hosts or concrete router services" >&2
  exit 1
fi

echo "router architecture guard passed"
