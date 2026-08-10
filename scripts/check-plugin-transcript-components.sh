#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

fail() {
  echo "Plugin transcript component guard failed: $*" >&2
  exit 1
}

for source in \
  plugins/document-plugin/src/document_tui.rs \
  plugins/git-plugin/src/git_tui.rs \
  plugins/ocr-plugin/src/ocr_tui.rs \
  plugins/question-plugin/src/question_outcome_tui.rs \
  plugins/web-search-plugin/src/web_search_tui.rs \
  plugins/worktree-plugin/src/lib.rs; do
  if ! rg -q 'ToolCardStyle' "$source" \
    || ! rg -q 'tool_card_header|tool_card_header_rows' "$source"; then
    fail "$source must use the shared themed tool-card recipe"
  fi
done

if ! rg -q 'source_viewer_rows_with_style' plugins/filesystem-plugin/src/filesystem_tui.rs \
  || ! rg -q 'tool_card_header' plugins/filesystem-plugin/src/filesystem_tui.rs \
  || ! rg -q 'diff_viewer_rows_with_style' plugins/filesystem-plugin/src/file_change_tui.rs; then
  fail "filesystem visuals must use shared tool-card, source-viewer, and diff-viewer recipes"
fi

if ! rg -q 'terminal_viewer_rows' plugins/shell-plugin/src/shell_run_tui.rs \
  || ! rg -q 'context\.theme\(\)' plugins/shell-plugin/src/shell_run_tui.rs; then
  fail "shell visuals must use the shared bounded terminal viewer and host theme"
fi

if ! rg -q 'tool_card_header' plugins/vim-edit-plugin/src/vim_edit_playback_tui.rs \
  || ! rg -q 'context\.theme\(\)' plugins/vim-edit-plugin/src/vim_edit_playback_tui.rs; then
  fail "Vim-edit visuals must use shared tool-card chrome and host theme"
fi

for source in \
  plugins/document-plugin/src/document_tui.rs \
  plugins/filesystem-plugin/src/filesystem_tui.rs \
  plugins/git-plugin/src/git_tui.rs \
  plugins/ocr-plugin/src/ocr_tui.rs \
  plugins/question-plugin/src/question_outcome_tui.rs \
  plugins/shell-plugin/src/shell_run_tui.rs \
  plugins/vim-edit-plugin/src/vim_edit_playback_tui.rs \
  plugins/web-search-plugin/src/web_search_tui.rs \
  plugins/worktree-plugin/src/lib.rs; do
  if ! rg -q 'context\.theme\(\)|tool_card_style\(context\)|worktree_tool_card_style\(context\)' "$source"; then
    fail "$source must consume renderer-owned host presentation"
  fi
done

if ! rg -q 'push_tool_invocation_fallback_rows\(rows, invocation\.as_deref\(\), item, width\)' \
  packages/tui/src/render.rs; then
  fail "generic invocation fallback rendering must remain available"
fi

if ! rg -q 'plugin_host.*routed_visual|routed_visual' packages/tui/src/render.rs; then
  fail "schema-specific plugin routing must remain in the host presentation path"
fi

echo "plugin transcript component guard passed"
