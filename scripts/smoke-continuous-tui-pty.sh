#!/usr/bin/env bash
set -euo pipefail
# Never builds. Pass explicit, mutually compatible test artifacts; runtime state is isolated.
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec python3 "${root}/scripts/smoke-continuous-tui-pty.py" "$@"
