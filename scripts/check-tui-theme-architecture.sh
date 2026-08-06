#!/usr/bin/env bash
set -euo pipefail

fail() { echo "TUI theme architecture guard failed: $*" >&2; exit 1; }

for theme in \
  packages/tui/themes/terminal-native.toml \
  packages/tui/themes/terminal-native-structured.toml \
  packages/tui/themes/bcode-dark.toml \
  packages/tui/themes/bcode-light.toml \
  packages/tui/themes/monochrome.toml \
  packages/tui/themes/high-contrast.toml \
  packages/tui/themes/nord.toml; do
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

if rg -n 'Color::Blue|Color::Green|Color::Red|Color::Magenta|Color::Cyan|Color::BrightBlack' \
  packages/tui/src/render.rs | rg 'TranscriptItemKind|push_assistant_rows|push_reasoning_rows|push_permission_request_rows|push_pending_submission_rows|statusline_spans|history_banner_rows' >/dev/null; then
  fail "transcript and status-line semantics must consume active resolved styles"
fi

if rg -n 'frame\.fill\(area, " ", Style::new\(\)\.fg\(Color::White\)\.bg\(Color::Black\)' \
  plugins/model-plugin/src/lib.rs \
  plugins/skills-plugin/src/lib.rs \
  plugins/worktree-plugin/src/lib.rs \
  plugins/workflow-plugin/src/tui.rs; then
  fail "migrated plugin command surfaces must consume renderer-owned semantic themes"
fi

if rg -n 'Color::|frame\.fill\(area, " ", Style::new\(\)' \
  plugins/ralph-plugin/src/lib.rs; then
  fail "migrated Ralph home surface must consume renderer-owned semantic themes"
fi

if rg -n 'Color::|frame\.fill\(area, " ", Style::new\(\)' \
  plugins/workflow-plugin/src/authoring_tui.rs; then
  fail "workflow authoring surface must consume renderer-owned semantic themes"
fi

if rg -n 'Color::|frame\.fill\(area, " ", Style::new\(\)' \
  plugins/code-review-plugin/src/code_review_home.rs; then
  fail "code-review home must consume renderer-owned semantic themes"
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

if rg -n 'color\.r, color\.g, color\.b|foreground_r|foreground_g|foreground_b' \
  packages/tui/src \
  packages/tui-components/src \
  plugins/filesystem-plugin/src \
  plugins/code-review-plugin/src \
  --glob '*.rs'; then
  fail "syntax theme propagation must preserve terminal, ANSI, indexed, and RGB colors"
fi

if rg -n 'added_row: style\("diff\.added_row"\)\.unwrap_or_else\(bmux_tui::style::Style::new\)|removed_row: style\("diff\.removed_row"\)\.unwrap_or_else\(bmux_tui::style::Style::new\)|diff\.added_emphasis[\s\S]*Modifier::UNDERLINE|diff\.removed_emphasis[\s\S]*Modifier::UNDERLINE' \
  packages/tui/src/theme.rs; then
  fail "missing diff theme roles must retain visible changed-row and intraline backgrounds"
fi

if rg -n 'frame\.fill\(area,[^\n]*Color::Black' \
  plugins/code-review-plugin/src/code_review_tui_render.rs; then
  fail "code-review full-screen canvas must use renderer-owned theme presentation"
fi

echo "TUI theme architecture guard passed"
