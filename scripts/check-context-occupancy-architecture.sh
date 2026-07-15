#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

fail() {
  echo "context occupancy architecture violation: $1" >&2
  exit 1
}

if rg -n 'latest_context_usage' packages >/dev/null; then
  fail 'normal paths must read the context_occupancy projection, not the latest raw event'
fi

if rg -n 'ContextUsageObserved \{ snapshot \}' packages/tui/src >/dev/null; then
  fail 'the TUI must not derive occupancy from raw context observations'
fi

if rg -n 'select\("events"\).*context_usage_observed' packages/server packages/tui >/dev/null; then
  fail 'server and TUI normal paths must not query raw occupancy events'
fi

echo 'context occupancy architecture checks passed'
