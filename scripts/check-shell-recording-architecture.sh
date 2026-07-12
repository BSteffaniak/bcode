#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

# Core packages may route generic tool streams/artifacts and plugin visuals, but must never
# interpret shell-owned recording schemas, recording keys, or terminal MIME types. Generic UI
# extension adapters may still register plugin-owned request visuals; durable recording semantics
# must remain exclusively in shell-owned code.
patterns='bcode\.shell\.recording|terminal_pty_stream|application/x-bcode-terminal'
violations="$({ grep -R --include='*.rs' -nE "$patterns" packages || true; } \
  | grep -v '^packages/tui/src/tests.rs:' || true)"
if [[ -n "$violations" ]]; then
  printf '%s\n' "$violations" >&2
  echo "shell/terminal artifact knowledge must remain in plugins/shell-plugin" >&2
  exit 1
fi

# Domain words alone are too broad for UI components, but core production code must not branch on
# terminal recording concepts. Generic names such as terminal dimensions in a generic stream event
# remain allowed; shell-owned recording/replay identifiers do not.
if grep -R --include='*.rs' -nE 'TerminalRecording|ShellRecording|TerminalFrame|PtyRecording' \
  packages/server packages/session packages/ipc packages/model packages/tool; then
  echo "core packages must remain agnostic to shell recording and terminal replay domains" >&2
  exit 1
fi

echo "shell recording architecture guard passed"
