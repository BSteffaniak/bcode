#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

python3 - <<'PY'
from pathlib import Path
import re

adapters = Path("packages/hyperchad/ui/src/pages/home/adapters.rs").read_text()
tests = Path("packages/hyperchad/ui/src/pages/home/tests.rs").read_text()
models = Path("packages/session-view/models/src/lib.rs").read_text()

registries = {}
for registry in ("ARTIFACT_ADAPTERS", "VISUAL_ADAPTERS"):
    start = adapters.index(registry)
    next_registry = adapters.find("pub(super) static ", start + 1)
    end = len(adapters) if next_registry == -1 else next_registry
    entries = set(re.findall(r'\("([^"]+)",\s*(\d+)\)', adapters[start:end]))
    if not entries:
        raise SystemExit(f"{registry} has no schema-versioned entries")
    registries[registry] = entries

artifact_count = len(registries["ARTIFACT_ADAPTERS"])
visual_count = len(registries["VISUAL_ADAPTERS"])
if artifact_count != 24 or visual_count != 25:
    raise SystemExit(
        f"adapter registry changed without updating exhaustive coverage: "
        f"artifacts={artifact_count} visuals={visual_count}"
    )

required_tests = {
    "every_registered_visual_adapter_has_a_fixture",
    "every_registered_artifact_adapter_has_a_fixture",
    "every_tool_transcript_kind_and_lifecycle_state_renders",
    "registered_adapters_reject_missing_required_fields_and_unsupported_versions",
    "rich_adapter_state_matrix_covers_empty_truncated_optional_error_and_fallbacks",
}
missing = sorted(name for name in required_tests if f"fn {name}(" not in tests)
if missing:
    raise SystemExit(f"adapter state coverage is incomplete: {missing}")

match = re.search(
    r"pub enum TranscriptViewItemKind \{(?P<body>.*?)\n\}",
    models,
    re.DOTALL,
)
if not match:
    raise SystemExit("could not locate TranscriptViewItemKind")
variants = {
    re.match(r"\s*([A-Z][A-Za-z0-9]+)", line).group(1)
    for line in match.group("body").splitlines()
    if re.match(r"\s*[A-Z][A-Za-z0-9]+\s*\{", line)
}
expected_variants = {
    "UserMessage",
    "AssistantMessage",
    "ReasoningMessage",
    "ToolInvocation",
    "ToolRequest",
    "Permission",
    "RuntimeWork",
    "Usage",
    "Compaction",
    "Interaction",
    "Skill",
    "SystemMessage",
    "ToolContribution",
}
if variants != expected_variants:
    raise SystemExit(
        f"transcript variants changed without exhaustive component coverage: "
        f"missing={sorted(expected_variants - variants)} extra={sorted(variants - expected_variants)}"
    )

print(
    "HyperChad adapter/transcript state guard passed "
    f"({artifact_count} artifact adapters, {visual_count} visual adapters, {len(variants)} transcript variants)"
)
PY
