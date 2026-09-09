#!/usr/bin/env bash
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
fail() { echo "Transcript viewport guard failed: $*" >&2; exit 1; }
if rg -n 'let appended_rows|saturating_sub\(appended' packages/tui/src/transcript_viewport.rs; then
  fail 'viewport space must derive from resolved geometry, not positive growth accounting'
fi
rg -q 'sync_with_anchor' packages/tui/src/app.rs || fail 'correspondence must precede tail exhaustion'
rg -q 'commit_transcript_presentation' packages/tui/src/root_program.rs || fail 'presenter acknowledgment must own anchor baseline'
rg -q 'validate_visual_anchors' packages/plugin-sdk/src/tui_visual.rs || fail 'plugin correspondence must be bounded'
rg -q 'push_routed_tool_surface' packages/tui/src/render.rs || fail 'tool lifecycle composition must remain unified'
if rg -n 'filesystem|shell_run|SHELL_RUN_SCHEMA' packages/tui/src/transcript_viewport.rs; then
  fail 'viewport must not interpret tool domains'
fi
if rg -n 'enum TranscriptScrollMode|scroll_mode: TranscriptScrollMode' packages/tui/src/app.rs; then
  fail 'application must not retain a second viewport intent authority'
fi
rg -q 'begin_transcript_presentation' packages/tui/src/transcript_projection.rs || fail 'preparation requires a navigation checkpoint'
echo 'Transcript viewport architecture guard passed'
