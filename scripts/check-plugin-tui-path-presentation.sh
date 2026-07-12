#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if rg -n 'visual_rows_with_context|std::env::current_dir\(\).*PluginTuiVisualRenderContext' packages plugins --glob '*.rs'; then
  echo "plugin TUI visuals must use the single complete render-context API" >&2
  exit 1
fi

if rg -n '\.(display\(\)|to_string_lossy\(\))' plugins --glob '*tui*.rs'; then
  echo "plugin TUI adapters must render paths through PluginTuiVisualRenderContext::display_path" >&2
  exit 1
fi
