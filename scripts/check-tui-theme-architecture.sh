#!/usr/bin/env bash
set -euo pipefail

fail() { echo "TUI theme architecture guard failed: $*" >&2; exit 1; }

for theme in \
  packages/tui/themes/terminal-native.toml \
  packages/tui/themes/terminal-native-structured.toml \
  packages/tui/themes/bcode-dark.toml \
  packages/tui/themes/bcode-light.toml \
  packages/tui/themes/monochrome.toml \
  packages/tui/themes/high-contrast.toml; do
  [[ -f "$theme" ]] || fail "missing required bundled theme: $theme"
done

if rg -n 'frame\.fill\([^\n]*Color::Black|bg\(Color::Black\)' \
  packages/tui/src/picker_render.rs \
  packages/tui/src/command_palette_render.rs; then
  fail "migrated host render paths must not force black backgrounds"
fi

if rg -n 'SyntaxHighlighter::new\(\)' \
  plugins/filesystem-plugin/src \
  --glob '*.rs'; then
  fail "filesystem plugin syntax must consume renderer-owned theme context"
fi

if rg -n 'syntax_palette:\s*None' \
  plugins/filesystem-plugin/src \
  --glob '*.rs'; then
  fail "filesystem source/diff adapters must not bypass the active syntax theme"
fi

if rg -n 'frame\.fill\(area,[^\n]*Color::Black' \
  plugins/code-review-plugin/src/code_review_tui_render.rs; then
  fail "code-review full-screen canvas must use renderer-owned theme presentation"
fi

echo "TUI theme architecture guard passed"
