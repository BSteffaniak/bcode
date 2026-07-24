#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

backend_manifest_users="$(
  rg -l 'mermaid-rs-renderer' --glob 'Cargo.toml' . \
    | sed 's#^\./##' \
    | sort
)"
expected_manifest_users="$(cat <<'EOF'
Cargo.toml
packages/mermaid-render/Cargo.toml
EOF
)"
if [[ "$backend_manifest_users" != "$expected_manifest_users" ]]; then
  echo "Mermaid backend dependency escaped the workspace/private adapter manifests" >&2
  diff -u <(printf '%s\n' "$expected_manifest_users") <(printf '%s\n' "$backend_manifest_users") >&2 || true
  exit 1
fi

backend_source_users="$(
  rg -l 'mermaid_rs_renderer' --glob '*.rs' . \
    | sed 's#^\./##' \
    | sort
)"
expected_backend_source_users="packages/mermaid-render/src/lib.rs"
if [[ "$backend_source_users" != "$expected_backend_source_users" ]]; then
  echo "Mermaid backend types or calls escaped bcode_mermaid_render" >&2
  diff -u <(printf '%s\n' "$expected_backend_source_users") <(printf '%s\n' "$backend_source_users") >&2 || true
  exit 1
fi

if rg -n 'mermaid_rs_renderer|mermaid-rs-renderer' packages/markdown-render packages/tui \
  --glob '*.rs' --glob 'Cargo.toml'; then
  echo "Markdown and TUI packages must depend only on the Bcode Mermaid contract" >&2
  exit 1
fi

echo "Mermaid renderer architecture guard passed"
