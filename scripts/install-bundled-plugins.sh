#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Packaging is shared with BMUX. Never build implementation cdylibs directly.
exec python3 "${root}/scripts/install-bundled-plugins.py" "$@"
