#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

fail() {
  printf 'application operation parity check failed: %s\n' "$1" >&2
  exit 1
}

doc="docs/application-operation-parity.md"
[[ -f "$doc" ]] || fail "$doc is missing"

python3 - "$doc" <<'PY'
from pathlib import Path
import re
import sys

DOC = Path(sys.argv[1]).read_text(encoding="utf-8")


def enum_variants(path: str, enum_name: str) -> list[str]:
    source = Path(path).read_text(encoding="utf-8")
    match = re.search(rf"(?:pub\s+)?enum\s+{re.escape(enum_name)}\s*\{{", source)
    if match is None:
        raise SystemExit(f"application operation parity check failed: {enum_name} not found in {path}")

    index = match.end()
    body_start = index
    depth = 1
    while depth:
        if index >= len(source):
            raise SystemExit(
                f"application operation parity check failed: unterminated {enum_name} in {path}"
            )
        character = source[index]
        depth += int(character == "{") - int(character == "}")
        index += 1

    variants: list[str] = []
    depth = 0
    for line in source[body_start : index - 1].splitlines():
        code = line.split("//", 1)[0]
        stripped = code.strip()
        if depth == 0:
            variant = re.match(r"^([A-Z][A-Za-z0-9_]*)(?:\s*\{|\s*\(|,)", stripped)
            if variant is not None:
                variants.append(variant.group(1))
        depth += code.count("{") - code.count("}")
    return variants


inventories = [
    ("packages/session-view/models/src/lib.rs", "SessionViewAction"),
    ("packages/session-view/models/src/lib.rs", "SessionViewActionOutcome"),
    ("packages/tui/src/slash_registry.rs", "BuiltinCommandId"),
    ("packages/hyperchad/ui/src/context.rs", "PresentationAction"),
    ("packages/cli/src/lib.rs", "Commands"),
]

for path, enum_name in inventories:
    heading = f"### `{enum_name}`" if enum_name != "Commands" else "### Top-level `Commands`"
    start = DOC.find(heading)
    if start < 0:
        raise SystemExit(
            f"application operation parity check failed: missing {enum_name} inventory heading"
        )
    end = DOC.find("\n### ", start + len(heading))
    if end < 0:
        end = DOC.find("\n## ", start + len(heading))
    section = DOC[start : len(DOC) if end < 0 else end]
    missing = [variant for variant in enum_variants(path, enum_name) if f"`{variant}`" not in section]
    if missing:
        raise SystemExit(
            "application operation parity check failed: "
            f"{enum_name} variants missing from {sys.argv[1]}: {', '.join(missing)}"
        )

required_phrases = [
    "Shared application",
    "Frontend user state",
    "Frontend local",
    "Offline/lifecycle",
    "authorization",
    "cancellation",
    "JSON Lines",
    "unknown schema",
]
for phrase in required_phrases:
    if phrase.lower() not in DOC.lower():
        raise SystemExit(
            f"application operation parity check failed: required coverage phrase missing: {phrase}"
        )

print("application operation parity inventory is complete for checked source enums")
PY

boundary_doc="docs/application-operation-boundary.md"
[[ -f "$boundary_doc" ]] || fail "$boundary_doc is missing"

for phrase in \
  'focused, server-owned operation modules' \
  'local IPC adapter owns' \
  'not durable resume protocols' \
  'Plugin workflows remain plugin-owned' \
  'future concrete adapter'; do
  if ! rg -Fqi "$phrase" "$boundary_doc"; then
    fail "$boundary_doc is missing required boundary coverage: $phrase"
  fi
done

if ! rg -Fq '[`application-operation-boundary.md`](application-operation-boundary.md)' "$doc"; then
  fail "$doc does not link the application operation boundary"
fi

raw_ipc_callers="$(
  rg -l 'bcode_ipc::Request|\bRequest::' packages --glob '*.rs' \
    | grep -Ev '^packages/(client|server|ipc|daemon-lifecycle)/|(^|/)tests?(/|\.rs$)' \
    || true
)"
if [[ -n "$raw_ipc_callers" ]]; then
  printf '%s\n' "$raw_ipc_callers" >&2
  fail "production callers outside IPC/client/server/lifecycle boundaries construct raw IPC requests"
fi
