#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

# Core packages may route generic tool streams/artifacts and plugin visuals, but must never
# interpret shell-owned recording schemas, recording keys, or terminal MIME types. Generic UI
# extension adapters may still register plugin-owned request visuals; durable recording semantics
# must remain exclusively in shell-owned code.
patterns='bcode\.shell\.recording|terminal_pty_stream|application/x-bcode-terminal|ResizeToolInvocation|ToolInvocationResized|active_terminal_tool|TerminalViewerFrame'
violations="$({ grep -R --include='*.rs' -nE "$patterns" packages || true; } \
  | grep -v '^packages/tui/src/tests.rs:' || true)"
if [[ -n "$violations" ]]; then
  printf '%s\n' "$violations" >&2
  echo "shell/terminal artifact knowledge must remain in plugins/shell-plugin" >&2
  exit 1
fi

# Generic active-artifact plumbing must remain domain-neutral. Shell schemas, recording keys,
# recording content types, and frame terminology may never appear in host/session/client code.
active_artifact_violations="$({ grep -R --include='*.rs' -nE 'ArtifactUpdate.*(shell|terminal|pty)|active_artifact.*(shell|terminal|pty)' packages || true; } \
  | grep -v '^packages/tui/src/tests.rs:' || true)"
if [[ -n "$active_artifact_violations" ]]; then
  printf '%s\n' "$active_artifact_violations" >&2
  echo "generic active-artifact plumbing must not interpret shell/terminal domains" >&2
  exit 1
fi

# New durable session writes must reject every live-only stream variant. Keep this list focused on
# the generic persistence boundary: shell/plugin implementations remain free to use generic live
# visuals and artifacts without teaching the session crate their schemas.
for variant in OutputDelta VisualUpdate ArtifactUpdate LegacyPresentation; do
  if ! grep -A8 'fn live_only_session_event_kind' packages/session/src/lib.rs \
    | grep -q "ToolInvocationStreamEvent::$variant"; then
    echo "durable session boundary must reject live-only stream variant $variant" >&2
    exit 1
  fi
done
if ! grep -q 'LiveEventPersistenceRejected' packages/session/src/lib.rs; then
  echo "durable session boundary must fail closed for live-only stream events" >&2
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
