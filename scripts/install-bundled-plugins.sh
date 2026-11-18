#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Packaging is shared with BMUX: plugin crates package themselves through its packager.
exec python3 "${root}/scripts/install-bundled-plugins.py" "$@"
