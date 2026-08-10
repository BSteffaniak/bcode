#!/usr/bin/env bash
set -euo pipefail

fail() { echo "TUI theme architecture guard failed: $*" >&2; exit 1; }

if rg -n 'update_writable_config|writable_config_path|BCODE_CONFIG_ENV' \
  packages/tui/src/chat_loop.rs \
  packages/tui/src/effects.rs; then
  fail "interactive theme selection must persist to user state, not declarative config"
fi

if rg -n -U 'pub fn set_tui_theme_selection[\s\S]{0,1200}update_writable_config' \
  packages/config/src/lib.rs; then
  fail "theme state persistence must not delegate to writable config mutation"
fi

if rg -n 'surface\.base' \
  packages/tui/src/theme.rs \
  packages/tui/themes \
  --glob '*.rs' \
  --glob '*.toml'; then
  fail "resolved canvas must consume the documented canvas semantic role"
fi

if ! rg -q 'frame\.fill\(layout\.area, " ", app\.presented_theme\(\)\.canvas\)' \
  packages/tui/src/render.rs; then
  fail "normal TUI rendering must fill the complete frame from resolved canvas presentation"
fi

if rg -n 'presented_theme\(\)\.background|semantic_state_theme\(\)\.background' \
  packages/tui/src \
  --glob '*.rs'; then
  fail "TUI presentation must use the resolved canvas role rather than an ambiguous background field"
fi

if ! rg -q 'fn render_with_theme' plugins/loop-plugin/src/lib.rs \
  || ! rg -q 'theme\.muted\.patch\(theme\.canvas\)' plugins/loop-plugin/src/lib.rs \
  || ! rg -q 'focused_border: theme\.focused' plugins/loop-plugin/src/lib.rs; then
  fail "loop plugin surface must consume renderer-owned theme presentation"
fi

if rg -n 'background\(theme\.text\)|frame\.fill\(area, " ", theme\.text\)|Style::new\(\)\.fg\(theme\.accent\)' \
  packages/tui/src/command_palette_render.rs \
  packages/tui/src/picker_render.rs \
  packages/tui/src/slash_palette_render.rs \
  packages/tui/src/thinking_dialog_render.rs; then
  fail "raised surfaces and focused controls must consume resolved semantic surface styles"
fi

if ! rg -q 'style\("surface\.raised"\)' packages/tui/src/theme.rs \
  || ! rg -q 'style\("surface\.overlay"\)' packages/tui/src/theme.rs \
  || ! rg -q 'style\("control\.focused"\)' packages/tui/src/theme.rs; then
  fail "resolved presentation must provide the bounded raised, overlay, and focused-control hierarchy"
fi

for theme in \
  packages/tui/themes/terminal-native.toml \
  packages/tui/themes/terminal-native-structured.toml \
  packages/tui/themes/bcode.toml \
  packages/tui/themes/bcode-dark.toml \
  packages/tui/themes/bcode-light.toml \
  packages/tui/themes/monochrome.toml \
  packages/tui/themes/high-contrast.toml \
  packages/tui/themes/nord.toml; do
  [[ -f "$theme" ]] || fail "missing required bundled theme: $theme"
done

for required_test in \
  explicit_rgb_bundled_themes_meet_text_contrast_thresholds \
  reduced_color_themes_retain_modifier_redundancy \
  opaque_picker_frame_exercises_modal_surface_and_selection_hierarchy \
  bundled_ids_copy_parse_and_future_schemas_fail_closed \
  external_schema_v1_theme_lifecycle_remains_compatible \
  theme_changes_invalidate_presentation_without_mutating_session_projection; do
  if ! rg -q "fn ${required_test}" packages/tui/src; then
    fail "missing durable TUI theme regression: ${required_test}"
  fi
done

if ! rg -q '"bcode:auto-dark"' packages/tui/src/render.rs \
  || ! rg -q '"bcode:auto-light"' packages/tui/src/render.rs \
  || ! rg -q '"nord"' packages/tui/src/render.rs; then
  fail "cross-theme semantic matrix must cover adaptive variants and Nord"
fi

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

# Every bundled plugin surface must accept renderer-owned presentation so new
# unthemed chrome cannot bypass the Phase 3 migration boundary.
while IFS= read -r surface_file; do
  if ! rg -q 'fn render_with_theme' "$surface_file"; then
    fail "bundled plugin surface lacks renderer-owned theme presentation: $surface_file"
  fi
done < <(rg -l 'impl (bcode_plugin_sdk::tui::)?PluginTuiSurface for' plugins --glob '*.rs' | sort)

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

if rg -n 'ThemeSet|classify_scope_color|SyntaxColor::Rgb\(101, 115, 126\)' \
  packages/syntax-render/src/lib.rs; then
  fail "syntax semantics must come from parser scopes rather than aesthetic theme colors"
fi

if rg -n 'themes\.themes\.values\(\)\.next\(\)' \
  packages/syntax-render/src/lib.rs; then
  fail "syntax classification must select its canonical theme deterministically"
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

if ! rg -q 'pub const fn component_theme\(self\) -> ComponentTheme' packages/tui/src/theme.rs \
  || ! rg -q 'let components = theme\.component_theme\(\)' packages/tui/src/render.rs; then
  fail "PresentedTheme must have one canonical BMUX ComponentTheme conversion used by plugin delivery"
fi

if rg -n 'ComponentTheme \{' packages/tui/src --glob '*.rs' \
  | rg -v 'packages/tui/src/theme.rs'; then
  fail "BMUX ComponentTheme construction must remain centralized at the TUI theme boundary"
fi

if ! rg -q 'component_theme_version: bcode_plugin_sdk::tui::PLUGIN_TUI_COMPONENT_THEME_VERSION' \
  packages/tui/src/render.rs; then
  fail "plugin theme delivery must include the canonical versioned component theme"
fi

if ! rg -q 'pub const fn component_theme\(&self\).*Option<bmux_tui_components::theme::ComponentTheme>' \
  packages/plugin-sdk/src/tui.rs; then
  fail "plugin component-theme access must remain compatibility-version checked"
fi

python3 - <<'PY'
from pathlib import Path

for source_path in [
    Path("plugins/document-plugin/src/document_tui.rs"),
    Path("plugins/git-plugin/src/git_tui.rs"),
    Path("plugins/ocr-plugin/src/ocr_tui.rs"),
    Path("plugins/question-plugin/src/question_outcome_tui.rs"),
    Path("plugins/web-search-plugin/src/web_search_tui.rs"),
    Path("plugins/worktree-plugin/src/lib.rs"),
    Path("plugins/model-plugin/src/lib.rs"),
    Path("plugins/skills-plugin/src/lib.rs"),
    Path("plugins/workflow-plugin/src/tui.rs"),
]:
    production = source_path.read_text().split("#[cfg(test)]", 1)[0]
    if "Style::new().fg(Color::" in production or "Style::default().fg(Color::" in production:
        raise SystemExit(
            f"{source_path}: migrated plugin transcript visuals must consume "
            "renderer-owned component theme roles"
        )
PY

echo "TUI theme architecture guard passed"
