#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if rg -n 'bmux_tui|bcode_tui|bcode_ipc|hyperchad|PluginTuiVisualAdapter|PluginTuiArtifactChunk' packages/plugin-sdk/src/tui_visual.rs; then
  echo "serialized TUI visual contracts must remain renderer-implementation-neutral" >&2
  exit 1
fi

if rg -n 'struct (RenderTuiVisual|SerializedTui)|enum SerializedTui|TUI_VISUAL_ADAPTER_INTERFACE_ID' packages/plugin-sdk/src/tui.rs; then
  echo "serialized TUI visual ABI types belong in the portable tui_visual contract module" >&2
  exit 1
fi

if rg -n 'visual_rows_with_context|std::env::current_dir\(\).*PluginTuiVisualRenderContext' packages plugins --glob '*.rs'; then
  echo "plugin TUI visuals must use the single complete render-context API" >&2
  exit 1
fi

# Direct path display in rendered plugin output must use the host context. Execution payloads and
# shell-command quoting preserve canonical path identity and are not presentation rendering.
if rg -n '\.(display\(\)|to_string_lossy\(\))' plugins --glob '*tui*.rs' \
  | rg -v 'workspace_snapshot:|workflow_path_identity|to_string_lossy|shell_quote_path|command: format!'; then
  echo "plugin TUI adapters must render paths through PluginTuiVisualRenderContext::display_path" >&2
  exit 1
fi
