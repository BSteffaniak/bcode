#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

python3 - <<'PY'
from pathlib import Path

for path in Path("packages").rglob("*.rs"):
    source = path.read_text()
    offset = 0
    while True:
        start = source.find("impl InvocationArtifactSink", offset)
        if start < 0:
            break
        open_brace = source.find("{", start)
        if open_brace < 0:
            raise SystemExit(f"unterminated artifact sink implementation: {path}")
        depth = 0
        end = None
        for index in range(open_brace, len(source)):
            if source[index] == "{":
                depth += 1
            elif source[index] == "}":
                depth -= 1
                if depth == 0:
                    end = index + 1
                    break
        if end is None:
            raise SystemExit(f"unterminated artifact sink implementation: {path}")
        block = source[start:end]
        signature_end = block.find(") -> InvocationCapabilityFuture")
        signature = block[:signature_end] if signature_end >= 0 else ""
        if "ArtifactCommitGuard" not in signature:
            raise SystemExit(
                f"artifact sink implementation does not accept ArtifactCommitGuard: {path}"
            )
        if (
            "ToolArtifactWriteResolution::Written" in block
            and ".commit(" not in block
            and "write_session_invocation_artifact(" not in block
        ):
            raise SystemExit(
                f"artifact sink can publish without consuming its commit guard: {path}"
            )
        offset = end
PY

if rg -n 'artifacts\.write\(request\)' packages/agent-runtime/src/turn.rs; then
  echo "runtime artifact routing bypasses ArtifactCommitGuard" >&2
  exit 1
fi

if ! rg -q 'artifacts\.write\(request, commit\)' packages/agent-runtime/src/turn.rs; then
  echo "runtime artifact routing is missing ArtifactCommitGuard" >&2
  exit 1
fi

echo "artifact final-commit guard passed"
