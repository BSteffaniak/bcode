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

if rg -n 'PICKER_BG|Color::Black|Color::BrightBlack|Color::Yellow|Color::White|Color::Rgb\(38, 52, 64\)' \
  packages/tui/src/picker_render.rs \
  packages/tui/src/model_picker_render.rs \
  packages/tui/src/provider_picker_render.rs \
  packages/tui/src/session_picker_render.rs \
  packages/tui/src/skill_picker_render.rs \
  packages/tui/src/worktree_picker_render.rs; then
  fail "migrated picker render paths must consume resolved semantic styles"
fi

if rg -n 'frame\.fill\([^\n]*Color::Black|bg\(Color::Black\)' \
  packages/tui/src/command_palette_render.rs; then
  fail "migrated command palette must not force black backgrounds"
fi

if rg -n 'MODAL_BG|Color::Black|ModalTheme::dark\(theme\.accent\)' \
  packages/tui/src/session_fork_dialog_render.rs \
  packages/tui/src/wt_create_dialog_render.rs \
  packages/tui/src/ralph_start_dialog_render.rs \
  packages/tui/src/thinking_dialog_render.rs \
  packages/tui/src/timeline_dialog_render.rs \
  packages/tui/src/theme_picker_render.rs; then
  fail "migrated dialogs must consume resolved modal and control styles"
fi

if rg -n 'Color::Black|Color::Yellow|Color::BrightWhite|Color::BrightBlack|ModalTheme::dark' \
  packages/tui/src/permission_dialog_render.rs \
  packages/tui/src/session_fork_flow.rs; then
  fail "permission and fork-prompt surfaces must consume resolved semantic styles"
fi

if rg -n 'Color::Black|Color::White|Color::Yellow|Color::Cyan|Color::BrightBlack|Color::Rgb\(38, 52, 64\)' \
  packages/tui/src/slash_palette_render.rs \
  packages/tui/src/model_picker.rs; then
  fail "slash and model picker rows must consume resolved semantic styles"
fi

if rg -n 'Color::Yellow|Color::Cyan|Color::Red' \
  packages/tui/src/render.rs | rg 'Stream status|push_tool_block_header' >/dev/null; then
  fail "stream and tool headers must consume active semantic styles"
fi

if rg -n 'Color::Black|Color::White|Color::Yellow|Color::Cyan|Color::Blue|Color::Red|Color::Green|Color::BrightBlack' \
  packages/tui/src/onboarding_render.rs \
  packages/tui/src/setup_board.rs; then
  fail "onboarding and setup-board surfaces must consume resolved semantic styles"
fi

if rg -n 'frame\.fill\(area, " ", Style::new\(\)\.fg\(Color::White\)\.bg\(Color::Black\)\)' \
  packages/tui/src/render.rs; then
  fail "Markdown source overlay must consume resolved semantic styles"
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
