#!/usr/bin/env bash
set -euo pipefail

fail() { echo "TUI Markdown projection architecture guard failed: $*" >&2; exit 1; }

projection_calls="$({ rg -n 'transcript_markdown_cache\(\)\.project' packages/tui/src --glob '*.rs' || true; })"
if [[ -n "$projection_calls" ]]; then
  # The deterministic frame harness deliberately installs accepted generations because it runs
  # without the production chat-loop worker. Production projection remains renderer-owned.
  unexpected="$(printf '%s\n' "$projection_calls" | rg -v 'packages/tui/src/(render|frame_sequence_harness)\.rs:' || true)"
  if [[ -n "$unexpected" ]]; then
    printf '%s\n' "$unexpected" >&2
    fail "transcript Markdown projection must remain owned by the renderer boundary"
  fi
fi

if rg -n 'bcode_markdown_render|MarkdownRender(Result|Options)|MarkdownProjection' \
  packages/session packages/session-view packages/frontend-models \
  --glob '*.rs' 2>/dev/null; then
  fail "terminal Markdown renderer and scheduling types must not enter portable session/frontend contracts"
fi

if ! rg -q 'transcript_markdown_projection_for_layout' packages/tui/src/transcript_projection.rs; then
  fail "transcript layout must consume the authoritative retained Markdown projection"
fi
if ! rg -q 'transcript_markdown_projection\(app, item, width\)' packages/tui/src/render.rs; then
  fail "Markdown semantic sidecars must consume the same retained projection as rows"
fi
if ! rg -q 'MarkdownProjectionCoordinator' packages/tui/src/chat_loop.rs; then
  fail "expensive transcript Markdown projection must remain off the input-critical TUI path"
fi

echo "TUI Markdown projection architecture guard passed"
