#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if grep -nE '^[[:space:]]*switchy(_database|_database_connection)?[[:space:]]*=' packages/metrics/Cargo.toml; then
  echo "metrics persistence must remain independent of instrumented database crates" >&2
  exit 1
fi

if grep -R --include='*.rs' -nE 'switchy(::database|_database)' packages/metrics/src; then
  echo "metrics persistence must not use instrumented database paths" >&2
  exit 1
fi

echo "metrics persistence architecture guard passed"
